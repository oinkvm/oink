use super::*;
use oink_foundation::Size;
use proptest::prelude::*;

const A: DeviceId = DeviceId([1; 32]);
const B: DeviceId = DeviceId([2; 32]);
const X: DeviceId = DeviceId([3; 32]);

fn sid(dev: DeviceId, key: &str) -> ScreenId {
    ScreenId::new(dev, key)
}

/// 本机坐标 = 视平面 plane × `upm`（每 plane 的本机单位）的屏幕。
fn screen(dev: DeviceId, key: &str, rect: Rect, upm: f64) -> ScreenPlacement {
    ScreenPlacement {
        id: sid(dev, key),
        name: key.into(),
        local: Rect::new(rect.x * upm, rect.y * upm, rect.w * upm, rect.h * upm),
        rect,
        size_source: SizeSource::Manual,

        primary: false,
        builtin: false,
    }
}

fn layout(screens: Vec<ScreenPlacement>) -> Layout {
    Layout {
        screens,
        ..Default::default()
    }
}

/// 视平面 plane 位置 → ScreenPos。
fn at(t: &Topology, id: &ScreenId, plane: Point) -> ScreenPos {
    ScreenPos {
        screen: id.clone(),
        local: t.from_plane(id, plane).unwrap(),
    }
}

fn plane(t: &Topology, p: &ScreenPos) -> Point {
    t.to_plane(p).unwrap()
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < (1e-6 / 686.0)
}

/// 浮点位置比较。
fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < (1e-3 / 686.0)
}

fn rect_of(l: &Layout, id: &ScreenId) -> Rect {
    l.screen(id).unwrap().rect
}

fn display(key: &str, bounds: Rect, px: (u32, u32), plane: Option<(f64, f64)>) -> DisplayInfo {
    DisplayInfo {
        key: key.into(),
        name: key.into(),
        bounds,
        pixel_width: px.0,
        pixel_height: px.1,
        reference_extent: plane.map(|(w, h)| Size::new(w, h)),
        rotation_deg: 0,
        is_primary: false,
        is_builtin: false,
        is_virtual: false,
    }
}

// ---------- 物理尺寸与自动排列 ----------

#[test]
fn physical_size_reported_estimated() {
    let good = display(
        "d",
        Rect::new(0.0, 0.0, 3840.0, 2160.0),
        (3840, 2160),
        Some(((597.0 / 686.0), (336.0 / 686.0))),
    );
    assert_eq!(estimate_reference_extent(&good).1, SizeSource::Reported);
    // 宽高比不对（投影仪常见的假尺寸）
    let bad = display(
        "d",
        Rect::new(0.0, 0.0, 3840.0, 2160.0),
        (3840, 2160),
        Some(((160.0 / 686.0), (160.0 / 686.0))),
    );
    let (s, src) = estimate_reference_extent(&bad);
    assert_eq!(src, SizeSource::Estimated);
    assert!((s.w - 3840.0 / 163.0 * (25.4 / 686.0)).abs() < (1e-9 / 686.0));
    // 没有尺寸
    let none = display("d", Rect::new(0.0, 0.0, 1920.0, 1080.0), (1920, 1080), None);
    let (s, src) = estimate_reference_extent(&none);
    assert_eq!(src, SizeSource::Estimated);
    assert!(s.w > (450.0 / 686.0) && s.w < (560.0 / 686.0), "24 寸左右：{s:?}");
    // 零尺寸
    let zero = display("d", Rect::new(0.0, 0.0, 1920.0, 1080.0), (1920, 1080), Some((0.0, 0.0)));
    assert_eq!(estimate_reference_extent(&zero).1, SizeSource::Estimated);
}

#[test]
fn auto_arranges_within_device_by_system_layout() {
    // Mac：内建屏 1512×982 pt，外接 4K 27 寸（点坐标 1920×1080）放在内建屏正上方、偏右。
    let mut builtin = display(
        "builtin",
        Rect::new(0.0, 0.0, 1512.0, 982.0),
        (3024, 1964),
        Some(((302.0 / 686.0), (196.0 / 686.0))),
    );
    builtin.is_primary = true;
    builtin.is_builtin = true;
    let ext = display(
        "ext",
        Rect::new(300.0, -1080.0, 1920.0, 1080.0),
        (3840, 2160),
        Some(((597.0 / 686.0), (336.0 / 686.0))),
    );
    let l = Layout::auto(&[(A, vec![builtin, ext])]);
    let b = l.screen(&sid(A, "builtin")).unwrap();
    let e = l.screen(&sid(A, "ext")).unwrap();
    assert!(b.primary && b.builtin && !e.primary && !e.builtin);
    assert!(near(e.rect.bottom(), b.rect.y), "外接屏在内建屏正上方");
    // 共享边段 [300, 1512] 的中点 906 在两块屏上对应同一个 x（plane）。
    let xb = b.local_to_plane(Point::new(906.0, 0.0)).x;
    let xe = e.local_to_plane(Point::new(906.0, 0.0)).x;
    assert!(near(xb, xe));
    // 两块屏之间形成传送门
    let t = l.topology(&Default::default());
    let r = t
        .step(
            &at(&t, &sid(A, "builtin"), Point::new(xb, b.rect.y + (5.0 / 686.0))),
            (0.0, -(10.0 / 686.0)),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(A, "ext"));
}

#[test]
fn auto_places_devices_side_by_side_and_syncs() {
    let mut mac = display(
        "m",
        Rect::new(0.0, 0.0, 1512.0, 982.0),
        (3024, 1964),
        Some(((302.0 / 686.0), (196.0 / 686.0))),
    );
    mac.is_primary = true;
    let win = display(
        "w",
        Rect::new(0.0, 0.0, 3840.0, 2160.0),
        (3840, 2160),
        Some(((597.0 / 686.0), (336.0 / 686.0))),
    );
    let mut rdp = display("rdp", Rect::new(3840.0, 0.0, 1920.0, 1080.0), (1920, 1080), None);
    rdp.is_virtual = true;
    let mut l = Layout::auto(&[(A, vec![mac.clone()]), (B, vec![win.clone(), rdp])]);
    let m = l.screen(&sid(A, "m")).unwrap().clone();
    let w = l.screen(&sid(B, "w")).unwrap().clone();
    assert!(near(w.rect.x, m.rect.right()));
    let group = bbox(l.screens.iter().filter(|s| s.id.device == B).map(|s| s.rect)).unwrap();
    assert!(near(group.center().y, m.rect.center().y));
    let mut owners = BTreeMap::new();
    let mut owner = OwnerLayoutPreferences::default();
    owner.screens.insert(
        "rdp".into(),
        ScreenPreference {
            remote_gain: 1.0,
            enabled: false,
            ..Default::default()
        },
    );
    owners.insert(B, owner);
    assert!(l.overlaps().is_empty());
    // 虚拟屏不参与传送门
    let t = l.topology(&owners);
    assert!(t.edges().iter().all(|e| e.segments.iter().all(|s| match &s.kind {
        SegmentKind::Portal { to, .. } => to.display != "rdp",
        SegmentKind::Wall => true,
    })));

    // 断开 w → detached，其他屏幕不动；重新接上 → 恢复原位置
    l.sync_device(B, &[]);
    assert!(l.screen(&sid(B, "w")).is_none());
    assert_eq!(rect_of(&l, &sid(A, "m")), m.rect);
    l.sync_device(B, std::slice::from_ref(&win));
    assert!(near(rect_of(&l, &sid(B, "w")).x, w.rect.x));

    // 分辨率变化：估算尺寸的屏幕按新分辨率重估，规整后仍与 w 相接
    let mut mac2 = mac.clone();
    mac2.reference_extent = None;
    mac2.bounds = Rect::new(0.0, 0.0, 1800.0, 1169.0);
    l.sync_device(A, &[mac2]);
    let m2 = l.screen(&sid(A, "m")).unwrap().clone();
    assert_eq!(m2.size_source, SizeSource::Estimated);
    assert_eq!(m2.local.w, 1800.0);
    assert_eq!(m2.rect.x, m.rect.x);
    assert!(near(m2.rect.w, m.rect.w * 1800.0 / 1512.0));
    assert!(near(m2.rect.right(), rect_of(&l, &sid(B, "w")).x) || near(m2.rect.bottom(), rect_of(&l, &sid(B, "w")).y));
    assert!(l.overlaps().is_empty());

    l.remove_device(B);
    assert!(l.screens.iter().all(|s| s.id.device == A));
}

#[test]
fn sync_adds_new_screen_next_to_existing() {
    let mut p = display(
        "p",
        Rect::new(0.0, 0.0, 1920.0, 1080.0),
        (1920, 1080),
        Some(((527.0 / 686.0), (296.0 / 686.0))),
    );
    p.is_primary = true;
    let mut l = Layout::auto(&[(A, vec![p.clone()])]);
    // 只有一块屏幕时可以随意移动
    l.move_screen(&sid(A, "p"), Point::new(1000.0 / 686.0, 500.0 / 686.0));
    let s2 = display(
        "s",
        Rect::new(1920.0, 0.0, 1920.0, 1080.0),
        (1920, 1080),
        Some(((527.0 / 686.0), (296.0 / 686.0))),
    );
    l.sync_device(A, &[p, s2]);
    let s = l.screen(&sid(A, "s")).unwrap();
    assert!(
        near(s.rect.x, 1527.0 / 686.0) && near(s.rect.y, 500.0 / 686.0),
        "{:?}",
        s.rect
    );
}

// ---------- 规整 ----------

/// 布局示例（物理布局）：上方 600/686 reference units 宽，下方两块 250/686 reference units，居中，中间隔 20/686 reference units，
/// 与上屏间隔 15/686 reference units。
fn research_physical() -> Layout {
    layout(vec![
        screen(A, "top", Rect::new(0.0, 0.0, 600.0 / 686.0, 340.0 / 686.0), 3.0 * 686.0),
        screen(
            B,
            "l",
            Rect::new(40.0 / 686.0, 355.0 / 686.0, 250.0 / 686.0, 160.0 / 686.0),
            4.0 * 686.0,
        ),
        screen(
            B,
            "r",
            Rect::new(310.0 / 686.0, 355.0 / 686.0, 250.0 / 686.0, 160.0 / 686.0),
            4.0 * 686.0,
        ),
    ])
}

fn research_example() -> Layout {
    let mut l = research_physical();
    // Routing tests below intentionally exercise positive gap travel.
    l.repair_overlaps();
    l
}

#[test]
fn normalize_research_example() {
    let mut l = research_example();
    l.normalize();
    // Close the inter-computer gap; preserve the OS-owned lower desktop gap.
    let (top, lft, rgt) = (
        rect_of(&l, &sid(A, "top")),
        rect_of(&l, &sid(B, "l")),
        rect_of(&l, &sid(B, "r")),
    );
    assert!(near(top.x, 0.0) && near(top.y, 15.0 / 686.0));
    assert!(near(lft.x, 40.0 / 686.0) && near(lft.y, 355.0 / 686.0), "{lft:?}");
    assert!(near(rgt.x, 310.0 / 686.0) && near(rgt.y, 355.0 / 686.0), "{rgt:?}");
    assert!(l.overlaps().is_empty());
    // 再规整一次不变
    let mut again = l.clone();
    again.normalize();
    for (p, q) in l.screens.iter().zip(&again.screens) {
        assert!(near(p.rect.x, q.rect.x) && near(p.rect.y, q.rect.y));
    }
}

#[test]
fn normalize_closes_staggered_three_computer_gap_at_first_contact() {
    let mut l = layout(vec![
        screen(A, "top", Rect::new(508., 0., 868., 488.), 1.),
        screen(B, "left", Rect::new(0., 525., 558., 361.), 1.),
        screen(X, "right", Rect::new(737., 543., 584., 377.), 1.),
    ]);
    let before = l.clone();
    l.normalize();
    assert_eq!(l.screens[0].rect, Rect::new(508., 37., 868., 488.));
    assert_eq!(l.screens[1..], before.screens[1..]);
    assert!(l.overlaps().is_empty());
    let compacted = l.clone();
    l.normalize();
    assert_eq!(l, compacted);
}

#[test]
fn normalize_grid_and_overlaps() {
    // 2×2 网格：边框间隙各不相同，右下有 5/686 reference units 的测量重叠（c、d 左右压住）。
    let mut l = layout(vec![
        screen(A, "a", Rect::new(0.0, 0.0, 600.0 / 686.0, 340.0 / 686.0), 3.0 * 686.0),
        screen(
            A,
            "b",
            Rect::new(620.0 / 686.0, 5.0 / 686.0, 600.0 / 686.0, 340.0 / 686.0),
            3.0 * 686.0,
        ),
        screen(
            B,
            "c",
            Rect::new(2.0 / 686.0, 370.0 / 686.0, 600.0 / 686.0, 340.0 / 686.0),
            3.0 * 686.0,
        ),
        screen(
            B,
            "d",
            Rect::new(597.0 / 686.0, 380.0 / 686.0, 600.0 / 686.0, 340.0 / 686.0),
            3.0 * 686.0,
        ),
    ]);
    l.normalize();
    let r = |k: &str| {
        let dev = if k == "a" || k == "b" { A } else { B };
        rect_of(&l, &sid(dev, k))
    };
    // Native overlaps within a computer are OS-owned and never rearranged.
    assert!(near(r("d").x, 597.0 / 686.0));
    assert!(near(r("b").x, 620.0 / 686.0));
    assert!(near(r("c").y, 370.0 / 686.0));
    assert!(r("d").y - r("b").bottom() > (1.0 / 686.0), "{:?} {:?}", r("b"), r("d"));
    // 沿边的相对位置不变（d 比 c 低 10/686 reference units）
    assert!(near(r("d").y - r("c").y, 10.0 / 686.0));
    assert!(l.overlaps().is_empty());
}

#[test]
fn overlap_repair_leaves_all_non_overlapping_positions_unchanged() {
    let mut l = layout(vec![
        screen(A, "a", Rect::new(0.0, 0.0, 300.0 / 686.0, 200.0 / 686.0), 2.0 * 686.0),
        screen(
            A,
            "b",
            Rect::new(300.0 / 686.0, 0.0, 300.0 / 686.0, 200.0 / 686.0),
            2.0 * 686.0,
        ),
        screen(
            B,
            "c",
            Rect::new(700.0 / 686.0, 10.0 / 686.0, 300.0 / 686.0, 200.0 / 686.0),
            2.0 * 686.0,
        ),
        screen(
            X,
            "far",
            Rect::new(3000.0 / 686.0, -(400.0 / 686.0), 500.0 / 686.0, 300.0 / 686.0),
            2.0 * 686.0,
        ),
    ]);
    let before = l.clone();
    l.repair_overlaps();
    assert_eq!(l, before);
    l.repair_overlaps_around(&sid(X, "far"));
    assert_eq!(l, before);
    l.remove_device(A);
    assert_eq!(l.screens.len(), 2);
    assert!(l.overlaps().is_empty());
}

#[test]
fn tiny_real_overlap_is_separated_only_by_its_depth_with_reference_pinned() {
    for pin in [sid(A, "a"), sid(B, "b")] {
        let mut l = layout(vec![
            screen(A, "a", Rect::new(0.0, 0.0, 600.0 / 686.0, 340.0 / 686.0), 3.0 * 686.0),
            screen(
                B,
                "b",
                Rect::new(80.0 / 686.0, 339.8 / 686.0, 400.0 / 686.0, 250.0 / 686.0),
                2.0 * 686.0,
            ),
        ]);
        let before = l.clone();
        l.normalize_around(&pin);
        assert_eq!(rect_of(&l, &pin), rect_of(&before, &pin));
        assert!(close(l.screens[0].rect.bottom(), l.screens[1].rect.y));
        for (a, b) in l.screens.iter().zip(&before.screens) {
            assert!(close(a.rect.x, b.rect.x));
            assert!((a.rect.y - b.rect.y).abs() <= (0.200001 / 686.0));
        }
        assert_no_overlap(&l).unwrap();
    }
}

// ---------- 传送门 ----------

fn portals(t: &Topology, id: &ScreenId, side: Side) -> Vec<(f64, f64, Option<String>)> {
    t.edges()
        .into_iter()
        .find(|e| &e.screen == id && e.side == side)
        .unwrap()
        .segments
        .into_iter()
        .map(|s| {
            (
                s.from,
                s.to,
                match s.kind {
                    SegmentKind::Portal { to, .. } => Some(to.display),
                    SegmentKind::Wall => None,
                },
            )
        })
        .collect()
}

fn round_segs(v: Vec<(f64, f64, Option<String>)>) -> Vec<(i64, i64, Option<String>)> {
    v.into_iter()
        .map(|(a, b, k)| ((a * 686.0).round() as i64, (b * 686.0).round() as i64, k))
        .collect()
}

#[test]
fn research_example_segments_and_mapping() {
    let t = research_example().topology(&Default::default());
    let top = sid(A, "top");
    let segs = round_segs(portals(&t, &top, Side::Bottom));
    // 下方两块之间保留的 20/686 reference units 缝在上屏底边上是墙（> 吸附 8/686 reference units）
    assert_eq!(
        segs,
        vec![
            (0, 40, None),
            (40, 290, Some("l".into())),
            (290, 310, None),
            (310, 560, Some("r".into())),
            (560, 600, None)
        ]
    );

    // x = 100/686 reference units 处向下穿出：落在左下屏顶边，x 不变。
    let r = t
        .step(
            &at(&t, &top, Point::new(100.0 / 686.0, 339.0 / 686.0)),
            (0.0, (5.0 / 686.0)),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(B, "l"));
    assert_eq!(r.crossed, vec![sid(B, "l")]);
    let p = plane(&t, &r.to);
    assert!(near(p.x, 100.0 / 686.0) && near(p.y, 359.0 / 686.0), "{p:?}");

    // x = 20/686 reference units 处：墙（大屏多出的部分），挡住 y，停在底边。
    let r = t
        .step(
            &at(&t, &top, Point::new(20.0 / 686.0, 330.0 / 686.0)),
            (0.0, (50.0 / 686.0)),
        )
        .unwrap();
    assert_eq!(r.to.screen, top);
    assert!(r.blocked_y && !r.blocked_x);
    assert!(plane(&t, &r.to).y < (340.0 / 686.0) && plane(&t, &r.to).y > (339.99 / 686.0));

    // 下屏向上：整条上边都是传送门，位置不变。
    let r = t
        .step(
            &at(&t, &sid(B, "r"), Point::new(400.0 / 686.0, 360.0 / 686.0)),
            (0.0, -(10.0 / 686.0)),
        )
        .unwrap();
    assert_eq!(r.to.screen, top);
    let p = plane(&t, &r.to);
    assert!(near(p.x, 400.0 / 686.0) && near(p.y, 335.0 / 686.0));
    // A native desktop gap is a wall, rather than a layout-created intra-OS portal.
    let r = t
        .step(
            &at(&t, &sid(B, "l"), Point::new(285.0 / 686.0, 400.0 / 686.0)),
            (10.0 / 686.0, 0.0),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(B, "l"));
    assert!(r.blocked_x);
}

#[test]
fn diagonal_motion_slides_along_wall() {
    let t = research_example().topology(&Default::default());
    let top = sid(A, "top");
    // 在墙段上斜向右下移动：y 被挡，x 继续。
    let r = t
        .step(
            &at(&t, &top, Point::new(10.0 / 686.0, 335.0 / 686.0)),
            ((20.0 / 686.0), (20.0 / 686.0)),
        )
        .unwrap();
    assert_eq!(r.to.screen, top);
    assert!(r.blocked_y);
    let p = plane(&t, &r.to);
    assert!(close(p.x, 30.0 / 686.0), "{p:?}");
}

#[test]
fn big_small_side_by_side() {
    // 左：600×340 显示器；右：300×200 笔记本，垂直居中。
    let t = layout(vec![
        screen(A, "mon", Rect::new(0.0, 0.0, 600.0 / 686.0, 340.0 / 686.0), 3.0 * 686.0),
        screen(
            B,
            "lap",
            Rect::new(600.0 / 686.0, 70.0 / 686.0, 300.0 / 686.0, 200.0 / 686.0),
            5.0 * 686.0,
        ),
    ])
    .topology(&Default::default());
    let segs = portals(&t, &sid(A, "mon"), Side::Right);
    assert_eq!(
        segs,
        vec![
            (0.0, (70.0 / 686.0), None),
            ((70.0 / 686.0), (270.0 / 686.0), Some("lap".into())),
            ((270.0 / 686.0), (340.0 / 686.0), None)
        ]
    );
    assert_eq!(
        portals(&t, &sid(B, "lap"), Side::Left),
        vec![((70.0 / 686.0), (270.0 / 686.0), Some("mon".into()))]
    );
    // 大屏上方向右：墙
    let r = t
        .step(
            &at(&t, &sid(A, "mon"), Point::new(590.0 / 686.0, 20.0 / 686.0)),
            ((30.0 / 686.0), 0.0),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(A, "mon"));
    assert!(r.blocked_x);
    // 中间：穿过，y 不变
    let r = t
        .step(
            &at(&t, &sid(A, "mon"), Point::new(590.0 / 686.0, 150.0 / 686.0)),
            ((30.0 / 686.0), 0.0),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(B, "lap"));
    let p = plane(&t, &r.to);
    assert!(close(p.x, 620.0 / 686.0) && close(p.y, 150.0 / 686.0));
    // 反向全部可回
    let r = t.step(&r.to, (-(30.0 / 686.0), 0.0)).unwrap();
    assert_eq!(r.to.screen, sid(A, "mon"));
    assert!(close(plane(&t, &r.to).x, 590.0 / 686.0));
}

#[test]
fn actual_overlap_does_not_create_a_portal_hidden_from_projection() {
    let l = layout(vec![
        screen(A, "a", Rect::new(0., 0., 1., 1.), 1.),
        screen(B, "b", Rect::new(0.999, 0., 1., 1.), 1.),
    ]);
    assert!(
        l.topology(&Default::default())
            .edges()
            .iter()
            .all(|e| e.segments.iter().all(|s| s.kind == SegmentKind::Wall))
    );
    assert!(
        crate::projection::edge_spans(l.screens[0].rect, &[(l.screens[1].id.clone(), l.screens[1].rect)]).is_empty()
    );
}

#[test]
fn any_gap_is_skipped() {
    // 桥接不设阈值：隔多远都能穿过去（未规整的布局也一样）。
    for gap in [0.0, (30.0 / 686.0), (100.0 / 686.0), (1000.0 / 686.0), -1e-12] {
        let t = layout(vec![
            screen(A, "a", Rect::new(0.0, 0.0, 500.0 / 686.0, 300.0 / 686.0), 2.0 * 686.0),
            screen(
                B,
                "b",
                Rect::new((500.0 / 686.0) + gap, 10.0 / 686.0, 500.0 / 686.0, 300.0 / 686.0),
                2.0 * 686.0,
            ),
        ])
        .topology(&Default::default());
        let r = t
            .step(
                &at(&t, &sid(A, "a"), Point::new(495.0 / 686.0, 100.0 / 686.0)),
                ((10.0 / 686.0), 0.0),
            )
            .unwrap();
        assert_eq!(r.to.screen, sid(B, "b"), "gap {gap}");
        assert!(
            close(plane(&t, &r.to).x, (505.0 / 686.0) + gap),
            "跳过 {gap} plane 间隙"
        );
    }
    // 沿边没有重叠：墙
    let t = layout(vec![
        screen(A, "a", Rect::new(0.0, 0.0, 500.0 / 686.0, 300.0 / 686.0), 2.0 * 686.0),
        screen(
            B,
            "b",
            Rect::new(600.0 / 686.0, 305.0 / 686.0, 500.0 / 686.0, 300.0 / 686.0),
            2.0 * 686.0,
        ),
    ])
    .topology(&Default::default());
    let r = t
        .step(
            &at(&t, &sid(A, "a"), Point::new(495.0 / 686.0, 100.0 / 686.0)),
            ((1000.0 / 686.0), 0.0),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(A, "a"));
    assert!(r.blocked_x);
}

#[test]
fn snap_extends_portal_to_edge_end() {
    // B 比 A 低 5/686 reference units：A 右边 [0,5] 的墙短于 8/686 reference units → 并入传送门；B 底部超出 A 的部分在 B 的左边上是墙。
    let t = layout(vec![
        screen(A, "a", Rect::new(0.0, 0.0, 500.0 / 686.0, 300.0 / 686.0), 2.0 * 686.0),
        screen(
            B,
            "b",
            Rect::new(500.0 / 686.0, 5.0 / 686.0, 500.0 / 686.0, 400.0 / 686.0),
            2.0 * 686.0,
        ),
    ])
    .topology(&Default::default());
    assert_eq!(
        portals(&t, &sid(A, "a"), Side::Right),
        vec![(0.0, (300.0 / 686.0), Some("b".into()))]
    );
    let r = t
        .step(
            &at(&t, &sid(A, "a"), Point::new(499.0 / 686.0, 2.0 / 686.0)),
            ((5.0 / 686.0), 0.0),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(B, "b"));
    assert!(close(plane(&t, &r.to).y, 5.0 / 686.0), "贴到目标边端点");
    let segs = portals(&t, &sid(B, "b"), Side::Left);
    assert_eq!(
        segs,
        vec![
            ((5.0 / 686.0), (300.0 / 686.0), Some("a".into())),
            ((300.0 / 686.0), (405.0 / 686.0), None)
        ]
    );
}

#[test]
fn narrow_wall_between_portals_is_snapped() {
    // 上屏底边：两个传送门之间 6/686 reference units 的墙（< 8/686 reference units 吸附容差）从中点劈开；20/686 reference units 的墙保留。
    let t = layout(vec![
        screen(A, "top", Rect::new(0.0, 0.0, 600.0 / 686.0, 340.0 / 686.0), 3.0 * 686.0),
        screen(
            B,
            "l",
            Rect::new(0.0, 340.0 / 686.0, 297.0 / 686.0, 160.0 / 686.0),
            4.0 * 686.0,
        ),
        screen(
            B,
            "r",
            Rect::new(303.0 / 686.0, 340.0 / 686.0, 297.0 / 686.0, 160.0 / 686.0),
            4.0 * 686.0,
        ),
    ])
    .topology(&Default::default());
    assert_eq!(
        portals(&t, &sid(A, "top"), Side::Bottom),
        vec![
            (0.0, (300.0 / 686.0), Some("l".into())),
            ((300.0 / 686.0), (600.0 / 686.0), Some("r".into()))
        ]
    );
    let t = layout(vec![
        screen(A, "top", Rect::new(0.0, 0.0, 600.0 / 686.0, 340.0 / 686.0), 3.0 * 686.0),
        screen(
            B,
            "l",
            Rect::new(0.0, 340.0 / 686.0, 290.0 / 686.0, 160.0 / 686.0),
            4.0 * 686.0,
        ),
        screen(
            B,
            "r",
            Rect::new(310.0 / 686.0, 340.0 / 686.0, 290.0 / 686.0, 160.0 / 686.0),
            4.0 * 686.0,
        ),
    ])
    .topology(&Default::default());
    assert_eq!(
        portals(&t, &sid(A, "top"), Side::Bottom)[1],
        ((290.0 / 686.0), (310.0 / 686.0), None)
    );
}

#[test]
fn walls_off_maps_whole_edge() {
    let mut l = layout(vec![
        screen(A, "mon", Rect::new(0.0, 0.0, 600.0 / 686.0, 340.0 / 686.0), 3.0 * 686.0),
        screen(
            B,
            "lap",
            Rect::new(600.0 / 686.0, 70.0 / 686.0, 300.0 / 686.0, 200.0 / 686.0),
            5.0 * 686.0,
        ),
    ]);
    l.settings.walls = false;
    let t = l.topology(&Default::default());
    assert_eq!(
        portals(&t, &sid(A, "mon"), Side::Right),
        vec![(0.0, (340.0 / 686.0), Some("lap".into()))]
    );
    let r = t
        .step(
            &at(&t, &sid(A, "mon"), Point::new(599.0 / 686.0, 10.0 / 686.0)),
            ((5.0 / 686.0), 0.0),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(B, "lap"));
    assert!(close(plane(&t, &r.to).y, 70.0 / 686.0));
}

/// 左、中、右三块屏横排，左右两块比中间高出 `over` plane（顶部）。
fn taller_sides(over: f64, dev: [DeviceId; 3]) -> Layout {
    layout(vec![
        screen(
            dev[0],
            "l",
            Rect::new(0.0, 0.0, 500.0 / 686.0, (300.0 / 686.0) + over),
            2.0 * 686.0,
        ),
        screen(
            dev[1],
            "c",
            Rect::new(500.0 / 686.0, over, 400.0 / 686.0, 300.0 / 686.0),
            2.0 * 686.0,
        ),
        screen(
            dev[2],
            "r",
            Rect::new(900.0 / 686.0, 0.0, 500.0 / 686.0, (300.0 / 686.0) + over),
            2.0 * 686.0,
        ),
    ])
}

#[test]
fn portal_does_not_pass_over_a_participating_screen() {
    // 左屏比中屏高出 15/686 reference units：高出的那段不越过中屏通到右屏，是墙（15/686 reference units > 吸附 8/686 reference units）。
    let l = taller_sides(15.0 / 686.0, [A, X, B]);
    let t = l.topology(&Default::default());
    assert_eq!(
        portals(&t, &sid(A, "l"), Side::Right),
        vec![
            (0.0, (15.0 / 686.0), None),
            ((15.0 / 686.0), (315.0 / 686.0), Some("c".into()))
        ]
    );
    assert_eq!(
        portals(&t, &sid(B, "r"), Side::Left),
        vec![
            (0.0, (15.0 / 686.0), None),
            ((15.0 / 686.0), (315.0 / 686.0), Some("c".into()))
        ]
    );
    // 指针在那段撞墙、沿墙滑动，不会跳到右屏
    let r = t
        .step(
            &at(&t, &sid(A, "l"), Point::new(495.0 / 686.0, 5.0 / 686.0)),
            ((50.0 / 686.0), 0.0),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(A, "l"));
    assert!(r.blocked_x);
    let e = t.exit_through_edge(
        &at(&t, &sid(A, "l"), Point::new(499.0 / 686.0, 5.0 / 686.0)),
        (4.0, 0.0),
    );
    assert!(e.is_none(), "{e:?}");

    // 中屏离线：不挡路，整条边照常通到右屏（跳过 400/686 reference units）
    let t = l.topology_online(&Default::default(), |d| *d != X);
    assert_eq!(
        portals(&t, &sid(A, "l"), Side::Right),
        vec![(0.0, (315.0 / 686.0), Some("r".into()))]
    );
    let r = t
        .step(
            &at(&t, &sid(A, "l"), Point::new(495.0 / 686.0, 5.0 / 686.0)),
            ((10.0 / 686.0), 0.0),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(B, "r"));
    // 未启用的屏幕（虚拟显示器）同样不挡路
    let dis = l.clone();
    let mut owners = BTreeMap::new();
    let mut owner = OwnerLayoutPreferences::default();
    owner.screens.insert(
        "c".into(),
        ScreenPreference {
            remote_gain: 1.0,
            enabled: false,
            ..Default::default()
        },
    );
    owners.insert(X, owner);
    assert_eq!(
        portals(&dis.topology(&owners), &sid(A, "l"), Side::Right),
        vec![(0.0, (315.0 / 686.0), Some("r".into()))]
    );
}

#[test]
fn passing_over_rule_with_snap_walls_off_and_local_screens() {
    // 只高出 5/686 reference units（< 吸附 8/686 reference units）：那段墙并入中屏的传送门，落到中屏的上角
    let t = taller_sides(5.0 / 686.0, [A, X, B]).topology(&Default::default());
    assert_eq!(
        portals(&t, &sid(A, "l"), Side::Right),
        vec![(0.0, (305.0 / 686.0), Some("c".into()))]
    );
    let r = t
        .step(
            &at(&t, &sid(A, "l"), Point::new(495.0 / 686.0, 2.0 / 686.0)),
            ((10.0 / 686.0), 0.0),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(X, "c"));
    assert!(close(plane(&t, &r.to).y, 5.0 / 686.0));

    // 关闭墙：15/686 reference units 那段也分给中屏（不是右屏）
    let mut l = taller_sides(15.0 / 686.0, [A, X, B]);
    l.settings.walls = false;
    let t = l.topology(&Default::default());
    assert_eq!(
        portals(&t, &sid(A, "l"), Side::Right),
        vec![(0.0, (315.0 / 686.0), Some("c".into()))]
    );
    let r = t
        .step(
            &at(&t, &sid(A, "l"), Point::new(495.0 / 686.0, 5.0 / 686.0)),
            ((10.0 / 686.0), 0.0),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(X, "c"));
    assert!(close(plane(&t, &r.to).y, 15.0 / 686.0), "贴到中屏上角");

    // 本机三块屏之间同一套规则
    let t = taller_sides(15.0 / 686.0, [A, A, A]).topology(&Default::default());
    assert_eq!(
        portals(&t, &sid(A, "l"), Side::Right),
        vec![
            (0.0, (15.0 / 686.0), None),
            ((15.0 / 686.0), (315.0 / 686.0), Some("c".into()))
        ]
    );
    let e = t.exit_through_edge(
        &at(&t, &sid(A, "l"), Point::new(499.0 / 686.0, 5.0 / 686.0)),
        (4.0, 0.0),
    );
    assert!(e.is_none());
}

#[test]
fn screen_above_a_gap_does_not_block() {
    // 显示器下方两台笔记本之间留 20/686 reference units 缝，笔记本上边与显示器下边有 3/686 reference units 测量重叠：显示器横跨这条缝，
    // 但沿边（y）与两台笔记本相对的范围只重叠 3/686 reference units（≤ overlap_tolerance），不挡路。
    let t = layout(vec![
        screen(A, "m", Rect::new(0.0, 0.0, 632.0 / 686.0, 360.0 / 686.0), 3.0 * 686.0),
        screen(
            B,
            "l",
            Rect::new(-(50.0 / 686.0), 357.0 / 686.0, 344.0 / 686.0, 222.0 / 686.0),
            2.0 * 686.0,
        ),
        screen(
            X,
            "r",
            Rect::new(314.0 / 686.0, 357.0 / 686.0, 344.0 / 686.0, 222.0 / 686.0),
            2.0 * 686.0,
        ),
    ])
    .topology(&Default::default());
    assert_eq!(
        portals(&t, &sid(B, "l"), Side::Right),
        vec![((357.0 / 686.0), (357.0 / 686.0 + 222.0 / 686.0), Some("r".into()))]
    );
}

#[test]
fn corner_only_is_not_crossed() {
    // B 只在 A 的右下角相接：斜着走过去被挡住（不做角落穿越）。
    let t = layout(vec![
        screen(A, "a", Rect::new(0.0, 0.0, 500.0 / 686.0, 300.0 / 686.0), 2.0 * 686.0),
        screen(
            B,
            "b",
            Rect::new(500.0 / 686.0, 300.0 / 686.0, 400.0 / 686.0, 300.0 / 686.0),
            2.0 * 686.0,
        ),
    ])
    .topology(&Default::default());
    let r = t
        .step(
            &at(&t, &sid(A, "a"), Point::new(495.0 / 686.0, 295.0 / 686.0)),
            ((20.0 / 686.0), (20.0 / 686.0)),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(A, "a"));
    assert!(r.blocked_x && r.blocked_y);
    assert!(portals(&t, &sid(A, "a"), Side::Right).iter().all(|s| s.2.is_none()));
}

#[test]
fn exact_corner_prefers_existing_portal() {
    // B 在 A 正下方（左对齐，比 A 窄）。
    let t = layout(vec![
        screen(A, "a", Rect::new(0.0, 0.0, 500.0 / 686.0, 300.0 / 686.0), 2.0 * 686.0),
        screen(
            B,
            "b",
            Rect::new(0.0, 300.0 / 686.0, 300.0 / 686.0, 200.0 / 686.0),
            2.0 * 686.0,
        ),
    ])
    .topology(&Default::default());
    let r = t
        .step(
            &at(&t, &sid(A, "a"), Point::new(290.0 / 686.0, 299.0 / 686.0)),
            ((2.0 / 686.0), (2.0 / 686.0)),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(B, "b"));
    let p = plane(&t, &r.to);
    assert!(close(p.x, 292.0 / 686.0) && close(p.y, 301.0 / 686.0), "{p:?}");
    // 正好从右下角点出发，同时到达右边与下边：两边在该处都是墙 → 停在角落。
    let corner = Point::new((500.0 / 686.0) - (1e-6 / 686.0), (300.0 / 686.0) - (1e-6 / 686.0));
    let r = t
        .step(&at(&t, &sid(A, "a"), corner), ((1.0 / 686.0), (1.0 / 686.0)))
        .unwrap();
    assert_eq!(r.to.screen, sid(A, "a"));
}

#[test]
fn local_screens_follow_same_rules() {
    // 同一台设备的两块屏，尺寸与密度不同。
    let l = layout(vec![
        screen(A, "1", Rect::new(0.0, 0.0, 600.0 / 686.0, 340.0 / 686.0), 3.2 * 686.0),
        screen(
            A,
            "2",
            Rect::new(600.0 / 686.0, 40.0 / 686.0, 530.0 / 686.0, 300.0 / 686.0),
            3.2 * 686.0,
        ),
    ]);
    let t = l.topology(&Default::default());
    let from = at(&t, &sid(A, "1"), Point::new(599.0 / 686.0, 200.0 / 686.0));
    // 本机光标在屏 1 右边继续向右 10 个单位
    let e = t.exit_through_edge(&from, (10.0, 0.0)).unwrap();
    assert_eq!(e.screen, sid(A, "2"));
    let p = plane(&t, &e);
    assert!(close(p.y, 200.0 / 686.0));
    assert!(
        close(p.x, (600.0 / 686.0) + (10.0 / 686.0) / 3.2 - (1.0 / 686.0)),
        "{p:?}"
    );
    // 在屏内移动：None；撞墙：None
    assert!(t.exit_through_edge(&from, (-10.0, 0.0)).is_none());
    let top = at(&t, &sid(A, "1"), Point::new(599.0 / 686.0, 10.0 / 686.0));
    assert!(t.exit_through_edge(&top, (10.0, 0.0)).is_none());
}

#[test]
fn gain_and_speed_modes() {
    let l = layout(vec![
        screen(A, "a", Rect::new(0.0, 0.0, 600.0 / 686.0, 340.0 / 686.0), 3.0 * 686.0),
        screen(
            B,
            "tv",
            Rect::new(600.0 / 686.0, 0.0, 1200.0 / 686.0, 680.0 / 686.0),
            2.0 * 686.0,
        ),
    ]);
    let mut owners = BTreeMap::new();
    let mut owner = OwnerLayoutPreferences::default();
    owner.screens.insert(
        "tv".into(),
        ScreenPreference {
            remote_gain: 1.0,
            gain: 2.0,
            ..Default::default()
        },
    );
    owners.insert(B, owner);
    let t = l.topology(&owners);
    assert!(close(
        t.reference_per_native_unit(&sid(A, "a")).unwrap(),
        (1.0 / 686.0) / 3.0
    ));
    assert!(close(t.reference_per_native_unit(&sid(B, "tv")).unwrap(), 0.25 / 686.0));
    // 从 a 右边 10/686 reference units 处向右 20 手部 plane：a 上走 10/686 reference units，剩余 10 手部 plane 在电视上 × 2 = 20/686 reference units。
    let r = t
        .step(
            &at(&t, &sid(A, "a"), Point::new(590.0 / 686.0, 100.0 / 686.0)),
            ((20.0 / 686.0), 0.0),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(B, "tv"));
    assert!(close(plane(&t, &r.to).x, 620.0 / 686.0));

    for id in [A, B] {
        owners.insert(
            id,
            OwnerLayoutPreferences {
                speed_mode: SpeedMode::ScreenProportional,
                ..Default::default()
            },
        );
    }
    let t = l.topology(&owners);
    // 按比例模式：电视对角线是 a 的两倍 → 同样的手部位移走两倍的视平面距离。
    let ka = t.plane_gain(&sid(A, "a")).unwrap();
    let kt = t.plane_gain(&sid(B, "tv")).unwrap();
    assert!(close(kt / ka, 2.0));
    let hand_a = (600.0 / 686.0) / ka;
    let r = t
        .step(
            &at(&t, &sid(B, "tv"), Point::new(600.0 / 686.0, 10.0 / 686.0)),
            (hand_a * 0.5, 0.0),
        )
        .unwrap();
    assert!(close(plane(&t, &r.to).x, (600.0 / 686.0) + (600.0 / 686.0)));
}

#[test]
fn plane_roundtrip_and_clamping() {
    let t = layout(vec![
        screen(A, "top", Rect::new(0.0, 0.0, 600.0 / 686.0, 340.0 / 686.0), 3.0 * 686.0),
        screen(
            B,
            "l",
            Rect::new(40.0 / 686.0, 340.0 / 686.0, 250.0 / 686.0, 160.0 / 686.0),
            4.0 * 686.0,
        ),
    ])
    .topology(&Default::default());
    let id = sid(B, "l");
    let pos = ScreenPos {
        screen: id.clone(),
        local: Point::new(200.0, 1400.0),
    };
    let plane = t.to_plane(&pos).unwrap();
    assert!(close(plane.x, 50.0 / 686.0) && close(plane.y, 350.0 / 686.0));
    let back = t.from_plane(&id, plane).unwrap();
    assert!(close(back.x, 200.0) && close(back.y, 1400.0));
    // 屏外的 plane 被裁剪进屏内
    let c = t.from_plane(&id, Point::new(-(1e9 / 686.0), 1e9 / 686.0)).unwrap();
    let s = t.screen(&id).unwrap();
    assert!(s.local.contains(c));
    let unknown = ScreenPos {
        screen: sid(B, "zz"),
        local: Point::default(),
    };
    assert!(t.to_plane(&unknown).is_none());
    assert!(t.step(&unknown, ((1.0 / 686.0), (1.0 / 686.0))).is_none());
}

#[test]
fn float_edge_cases() {
    let t = research_example().topology(&Default::default());
    let top = sid(A, "top");
    // 极小位移停在原屏
    let p = at(&t, &top, Point::new(599.999999 / 686.0, 100.0 / 686.0));
    let r = t.step(&p, ((1e-12 / 686.0), 0.0)).unwrap();
    assert_eq!(r.to.screen, top);
    // 非有限位移：不动
    let r = t.step(&p, (f64::NAN, (1.0 / 686.0))).unwrap();
    assert_eq!(r.to, p);
    let r = t.step(&p, (f64::INFINITY, 0.0)).unwrap();
    assert_eq!(r.to, p);
    // 巨大位移：仍在某块屏幕内
    for d in [
        ((1e12 / 686.0), (1e12 / 686.0)),
        (-(1e12 / 686.0), (3e11 / 686.0)),
        (0.0, (1e15 / 686.0)),
    ] {
        let r = t.step(&p, d).unwrap();
        let s = t.screen(&r.to.screen).unwrap();
        assert!(s.local.contains(r.to.local), "{d:?} → {r:?}");
    }
}

// ---------- 在线设备 ----------

/// A | X | B 横排，三台设备。
fn three_in_a_row() -> Layout {
    layout(vec![
        screen(A, "a", Rect::new(0.0, 0.0, 500.0 / 686.0, 300.0 / 686.0), 2.0 * 686.0),
        screen(
            X,
            "x",
            Rect::new(500.0 / 686.0, 0.0, 400.0 / 686.0, 300.0 / 686.0),
            3.0 * 686.0,
        ),
        screen(
            B,
            "b",
            Rect::new(900.0 / 686.0, 20.0 / 686.0, 500.0 / 686.0, 300.0 / 686.0),
            2.0 * 686.0,
        ),
    ])
}

#[test]
fn offline_middle_device_is_skipped_and_restored() {
    let l = three_in_a_row();
    let online = |d: &DeviceId| *d != X;
    let t = l.topology_online(&Default::default(), online);
    assert!(!t.is_active(&sid(X, "x")) && t.is_active(&sid(A, "a")));
    // A 的右边直接通到 B（跳过 X 的 400/686 reference units）
    assert_eq!(
        portals(&t, &sid(A, "a"), Side::Right),
        vec![
            (0.0, (20.0 / 686.0), None),
            ((20.0 / 686.0), (300.0 / 686.0), Some("b".into()))
        ]
    );
    let r = t
        .step(
            &at(&t, &sid(A, "a"), Point::new(495.0 / 686.0, 100.0 / 686.0)),
            ((10.0 / 686.0), 0.0),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(B, "b"));
    assert_eq!(r.crossed, vec![sid(B, "b")]);
    assert!(close(plane(&t, &r.to).x, 905.0 / 686.0) && close(plane(&t, &r.to).y, 100.0 / 686.0));
    // 反向回到 A
    let back = t.step(&r.to, (-(10.0 / 686.0), 0.0)).unwrap();
    assert_eq!(back.to.screen, sid(A, "a"));
    // 本机光标同样
    let e = t
        .exit_through_edge(&at(&t, &sid(A, "a"), Point::new(499.0, 100.0)), (4.0, 0.0))
        .unwrap();
    assert_eq!(e.screen, sid(B, "b"));
    // 离线屏幕仍在布局与查询结构里（界面画在原位置）
    assert!(t.screen(&sid(X, "x")).is_some());
    let v = l.view(&Default::default(), online);
    let xv = v.screens.iter().find(|s| s.device == X).unwrap();
    assert!(!xv.online && xv.rect == Rect::new(500.0 / 686.0, 0.0, 400.0 / 686.0, 300.0 / 686.0));
    assert!(v.screens.iter().filter(|s| s.device != X).all(|s| s.online));

    // 重新上线：A → X → B
    let t = l.topology_online(&Default::default(), |_| true);
    let r = t
        .step(
            &at(&t, &sid(A, "a"), Point::new(495.0 / 686.0, 100.0 / 686.0)),
            ((10.0 / 686.0), 0.0),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(X, "x"));
    let r = t.step(&r.to, ((400.0 / 686.0), 0.0)).unwrap();
    assert_eq!(r.to.screen, sid(B, "b"));
}

#[test]
fn offline_device_at_the_edge_becomes_wall() {
    let l = three_in_a_row();
    let t = l.topology_online(&Default::default(), |d| *d != B);
    assert!(portals(&t, &sid(X, "x"), Side::Right).iter().all(|s| s.2.is_none()));
    let r = t
        .step(
            &at(&t, &sid(X, "x"), Point::new(850.0 / 686.0, 150.0 / 686.0)),
            ((200.0 / 686.0), 0.0),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(X, "x"));
    assert!(r.blocked_x);
    // B 自己的边全是墙
    assert!(portals(&t, &sid(B, "b"), Side::Left).iter().all(|s| s.2.is_none()));
}

#[test]
fn only_local_screens_when_others_offline() {
    // 本机 A 两块屏；B、X 都离线。
    let l = layout(vec![
        screen(A, "1", Rect::new(0.0, 0.0, 500.0 / 686.0, 300.0 / 686.0), 2.0 * 686.0),
        screen(
            A,
            "2",
            Rect::new(500.0 / 686.0, 0.0, 500.0 / 686.0, 300.0 / 686.0),
            2.0 * 686.0,
        ),
        screen(
            X,
            "x",
            Rect::new(1000.0 / 686.0, 0.0, 400.0 / 686.0, 300.0 / 686.0),
            3.0 * 686.0,
        ),
        screen(
            B,
            "b",
            Rect::new(0.0, 300.0 / 686.0, 1000.0 / 686.0, 300.0 / 686.0),
            2.0 * 686.0,
        ),
    ]);
    let t = l.topology_online(&Default::default(), |d| *d == A);
    let mut pos = at(&t, &sid(A, "1"), Point::new(250.0 / 686.0, 150.0 / 686.0));
    for d in [
        ((400.0 / 686.0), 0.0),
        ((600.0 / 686.0), 0.0),
        (0.0, (500.0 / 686.0)),
        (-(2000.0 / 686.0), -(300.0 / 686.0)),
        ((1500.0 / 686.0), (800.0 / 686.0)),
    ] {
        let r = t.step(&pos, d).unwrap();
        assert_eq!(r.to.screen.device, A, "{d:?}");
        pos = r.to;
    }
    let r = t
        .step(
            &at(&t, &sid(A, "1"), Point::new(450.0 / 686.0, 150.0 / 686.0)),
            ((100.0 / 686.0), 0.0),
        )
        .unwrap();
    assert_eq!(r.to.screen, sid(A, "2"));
    for e in t.edges() {
        for s in &e.segments {
            if let SegmentKind::Portal { to, .. } = &s.kind {
                assert_eq!(to.device, A);
                assert_eq!(e.screen.device, A);
            }
        }
    }
}

// ---------- 编辑 ----------

#[test]
fn move_screen_preserves_free_positions_and_only_resolves_overlap() {
    let mut l = layout(vec![
        screen(A, "a", Rect::new(0.0, 0.0, 500.0 / 686.0, 300.0 / 686.0), 2.0 * 686.0),
        screen(
            B,
            "b",
            Rect::new(500.0 / 686.0, 0.0, 400.0 / 686.0, 250.0 / 686.0),
            2.0 * 686.0,
        ),
    ]);
    // 保留底边 40/686 reference units 的缝，x 也不吸附
    assert_eq!(
        l.drop_position(&sid(B, "b"), Point::new(70.0 / 686.0, 340.0 / 686.0)),
        Some(Point::new(70.0 / 686.0, 340.0 / 686.0))
    );
    // 接近中线也不对齐
    let dropped = l
        .drop_position(&sid(B, "b"), Point::new(60.0 / 686.0, 340.0 / 686.0))
        .unwrap();
    assert!(close(dropped.x, 60.0 / 686.0) && close(dropped.y, 340.0 / 686.0));
    // 接近左边也不对齐
    let dropped = l
        .drop_position(&sid(B, "b"), Point::new(8.0 / 686.0, 320.0 / 686.0))
        .unwrap();
    assert!(close(dropped.x, 8.0 / 686.0) && close(dropped.y, 320.0 / 686.0));
    // 拖到与 a 重叠的地方：贴到最近的一条边外面
    assert_eq!(
        l.drop_position(&sid(B, "b"), Point::new(450.0 / 686.0, 100.0 / 686.0)),
        Some(Point::new(500.0 / 686.0, 100.0 / 686.0))
    );
    // 拖到远处：保持原始放下位置
    let p = l
        .drop_position(&sid(B, "b"), Point::new(3000.0 / 686.0, 120.0 / 686.0))
        .unwrap();
    assert_eq!(p, Point::new(3000.0 / 686.0, 120.0 / 686.0));
    // 预览不修改布局
    let before = l.clone();
    let pv = l
        .preview_move(&sid(B, "b"), Point::new(70.0 / 686.0, 340.0 / 686.0))
        .unwrap();
    assert_eq!(l, before);
    assert!(l.move_screen(&sid(B, "b"), Point::new(70.0 / 686.0, 340.0 / 686.0)));
    assert_eq!(l, pv);
    let b = rect_of(&l, &sid(B, "b"));
    let a = rect_of(&l, &sid(A, "a"));
    assert_eq!(
        b,
        Rect::new(70.0 / 686.0, 340.0 / 686.0, 400.0 / 686.0, 250.0 / 686.0),
        "被拖的屏幕留在放下的位置"
    );
    assert_eq!(a, Rect::new(0.0, 0.0, 500.0 / 686.0, 300.0 / 686.0));
    assert!(l.overlaps().is_empty());
    assert!(!l.move_screen(&sid(B, "zz"), Point::default()));
}

#[test]
fn moving_screen_out_of_a_row_preserves_the_hole() {
    let mut l = layout(vec![
        screen(A, "a", Rect::new(0.0, 0.0, 500.0 / 686.0, 300.0 / 686.0), 2.0 * 686.0),
        screen(
            X,
            "x",
            Rect::new(500.0 / 686.0, 0.0, 400.0 / 686.0, 300.0 / 686.0),
            2.0 * 686.0,
        ),
        screen(
            B,
            "b",
            Rect::new(900.0 / 686.0, 0.0, 500.0 / 686.0, 300.0 / 686.0),
            2.0 * 686.0,
        ),
    ]);
    // 把中间的 x 拖到 a 的下方
    l.move_screen(&sid(X, "x"), Point::new(50.0 / 686.0, 320.0 / 686.0));
    let (a, x, b) = (
        rect_of(&l, &sid(A, "a")),
        rect_of(&l, &sid(X, "x")),
        rect_of(&l, &sid(B, "b")),
    );
    assert!(near(b.x, 900.0 / 686.0), "a、b 原地保留空位：{a:?} {b:?}");
    assert!(
        near(x.x, 50.0 / 686.0) && near(x.y, 320.0 / 686.0),
        "被拖的屏幕留在放下的位置：{x:?}"
    );
    assert!(near(a.x, 0.0) && near(a.y, 0.0), "其他屏幕不动");
    assert!(l.overlaps().is_empty());
}

#[test]
fn move_device_moves_all_its_screens() {
    let mut l = layout(vec![
        screen(A, "a", Rect::new(0.0, 0.0, 500.0 / 686.0, 300.0 / 686.0), 2.0 * 686.0),
        screen(
            B,
            "b1",
            Rect::new(500.0 / 686.0, 0.0, 400.0 / 686.0, 300.0 / 686.0),
            2.0 * 686.0,
        ),
        screen(
            B,
            "b2",
            Rect::new(900.0 / 686.0, 0.0, 400.0 / 686.0, 300.0 / 686.0),
            2.0 * 686.0,
        ),
    ]);
    // 把设备 B 整体拖到 a 的左边
    assert!(l.move_device(B, Point::new(-(1310.0 / 686.0), 30.0 / 686.0)));
    let (a, b1, b2) = (
        rect_of(&l, &sid(A, "a")),
        rect_of(&l, &sid(B, "b1")),
        rect_of(&l, &sid(B, "b2")),
    );
    assert!(near(b2.right(), a.x - (10.0 / 686.0)) && near(b1.right(), b2.x));
    assert!(near(b1.y - a.y, 30.0 / 686.0) && near(b2.y, b1.y));
    assert!(!l.move_device(X, Point::default()));
}

#[test]
fn overlapping_device_drop_moves_only_the_dragged_group_to_a_legal_edge() {
    let mut l = layout(vec![
        screen(A, "a", Rect::new(0.0, 0.0, 500.0 / 686.0, 300.0 / 686.0), 2.0 * 686.0),
        screen(
            B,
            "b1",
            Rect::new(600.0 / 686.0, 50.0 / 686.0, 100.0 / 686.0, 100.0 / 686.0),
            2.0 * 686.0,
        ),
        screen(
            B,
            "b2",
            Rect::new(750.0 / 686.0, 50.0 / 686.0, 100.0 / 686.0, 100.0 / 686.0),
            2.0 * 686.0,
        ),
    ]);
    let a = l.screens[0].clone();
    let preview = l.preview_move_device(B, Point::new(-(150.2 / 686.0), 0.0)).unwrap();
    assert!(l.move_device(B, Point::new(-(150.2 / 686.0), 0.0)));
    assert_eq!(l, preview);
    assert_eq!(l.screens[0], a);
    assert_eq!(
        l.screens[1].rect,
        Rect::new(500.0 / 686.0, 50.0 / 686.0, 100.0 / 686.0, 100.0 / 686.0)
    );
    assert_eq!(
        l.screens[2].rect,
        Rect::new(650.0 / 686.0, 50.0 / 686.0, 100.0 / 686.0, 100.0 / 686.0)
    );
    assert_no_overlap(&l).unwrap();
    // A sub-reference unit overlap is pushed out by exactly that amount.
    let id = sid(B, "b1");
    assert_eq!(
        l.drop_position(&id, Point::new(499.8 / 686.0, 50.0 / 686.0)),
        Some(Point::new(500.0 / 686.0, 50.0 / 686.0))
    );
}

#[test]
fn new_display_preserves_native_group_and_moves_only_colliding_computers() {
    let own = display(
        "own",
        Rect::new(0.0, 0.0, 1000.0, 600.0),
        (1000, 600),
        Some(((300.0 / 686.0), (180.0 / 686.0))),
    );
    let peer = display(
        "peer",
        Rect::new(0.0, 0.0, 1000.0, 600.0),
        (1000, 600),
        Some(((300.0 / 686.0), (180.0 / 686.0))),
    );
    let mut l = Layout::auto(&[(A, vec![own.clone()]), (B, vec![peer])]);
    let original = l.screens.clone();
    let second = display(
        "second",
        Rect::new(1000.0, 0.0, 1000.0, 600.0),
        (1000, 600),
        Some(((300.0 / 686.0), (180.0 / 686.0))),
    );
    l.sync_device(A, &[own.clone(), second]);
    assert_eq!(l.screen(&sid(A, "own")), Some(&original[0]));
    assert!(near(rect_of(&l, &sid(A, "second")).x, 300.0 / 686.0));
    assert_no_overlap(&l).unwrap();
    l.sync_device(A, &[own]);
    assert_eq!(l.screen(&sid(A, "own")), Some(&original[0]));
}

#[test]
fn set_physical_size_renormalizes() {
    let mut l = three_in_a_row();
    l.set_extent(&sid(X, "x"), Size::new(600.0 / 686.0, 340.0 / 686.0));
    assert!(l.overlaps().is_empty());
    let (x, b) = (rect_of(&l, &sid(X, "x")), rect_of(&l, &sid(B, "b")));
    assert!(near(x.w, 600.0 / 686.0));
    assert!(near(x.h, 450.0 / 686.0));
    assert!(near(b.x, x.right()));
    assert_eq!(l.screen(&sid(X, "x")).unwrap().size_source, SizeSource::Manual);
    assert_eq!(
        l.screen_at(Point::new(x.x + (1.0 / 686.0), x.y + (1.0 / 686.0))),
        Some(&sid(X, "x"))
    );
}

#[test]
fn resizing_is_uniform_for_both_inputs_and_clamps_the_whole_screen() {
    let mut l = three_in_a_row();
    let id = sid(X, "x");
    let original = rect_of(&l, &id);
    let ratio = original.w / original.h;
    assert!(l.resize_screen(&id, Size::new(original.w * 1.5, 111.0 / 686.0)));
    let width_edit = rect_of(&l, &id);
    assert!(near(width_edit.w, original.w * 1.5));
    assert!(near(width_edit.w / width_edit.h, ratio));
    assert!(l.resize_screen(&id, Size::new(width_edit.w, width_edit.h * 0.8)));
    let height_edit = rect_of(&l, &id);
    assert!(near(height_edit.h, width_edit.h * 0.8));
    assert!(near(height_edit.w / height_edit.h, ratio));
    assert!(l.resize_screen(&id, Size::new(1.0 / 686.0, 1.0 / 686.0)));
    let minimum = rect_of(&l, &id);
    assert!(near(minimum.w.min(minimum.h), 50.0 / 686.0));
    assert!(near(minimum.w / minimum.h, ratio));
    assert!(l.resize_screen(&id, Size::new(1e9 / 686.0, 1e9 / 686.0)));
    let maximum = rect_of(&l, &id);
    assert!(near(maximum.w.max(maximum.h), 4000.0 / 686.0));
    assert!(near(maximum.w / maximum.h, ratio));
}

#[test]
fn view_lists_screens() {
    let mut l = research_example();
    l.screens[0].primary = true;
    l.screens[1].builtin = true;
    let v = l.view(&Default::default(), |_| true);
    assert_eq!(v.screens.len(), 3);
    assert!(v.screens[0].primary && v.screens[1].builtin && !v.screens[2].primary);
    assert_eq!(v.screens[1].name, "l");
    let bb = v.bounds.unwrap();
    assert!(near(bb.w, 600.0 / 686.0) && near(bb.h, 515.0 / 686.0));
    let json = serde_json::to_string(&v).unwrap();
    assert!(json.contains("\"online\":true"));
}

#[test]
fn serde_roundtrip() {
    let l = research_example();
    let json = serde_json::to_string(&l).unwrap();
    let back: Layout = serde_json::from_str(&json).unwrap();
    assert_eq!(back, l);
    let edges = serde_json::to_string(&l.topology(&Default::default()).edges()).unwrap();
    assert!(edges.contains("Portal"));
}

// ---------- 性质测试 ----------

/// 随机布局：屏幕放在 10/686 reference units 网格上，容易产生相邻、间隙与重叠（未规整）。
fn arb_screens() -> impl Strategy<Value = Vec<ScreenPlacement>> {
    prop::collection::vec(
        (
            0i32..8,
            0i32..6,
            20i32..70,
            15i32..45,
            prop::sample::select(vec![1.0, 2.0, 3.7]),
            0u8..3,
        ),
        1..7,
    )
    .prop_map(|v| {
        v.iter()
            .enumerate()
            .map(|(i, &(gx, gy, w, h, upm, dev))| {
                screen(
                    [A, B, X][dev as usize],
                    &format!("s{i}"),
                    Rect::new(
                        f64::from(gx) * (100.0 / 686.0),
                        f64::from(gy) * (80.0 / 686.0),
                        f64::from(w) * (10.0 / 686.0),
                        f64::from(h) * (10.0 / 686.0),
                    ),
                    upm,
                )
            })
            .collect()
    })
}

fn arb_layout() -> impl Strategy<Value = Layout> {
    (arb_screens(), any::<bool>()).prop_map(|(screens, walls)| {
        let mut l = layout(screens);
        l.settings.walls = walls;
        l
    })
}

/// 规整后的随机布局。
fn arb_logical() -> impl Strategy<Value = Layout> {
    arb_layout().prop_map(|mut l| {
        l.normalize();
        l
    })
}

fn assert_no_overlap(l: &Layout) -> Result<(), TestCaseError> {
    for i in 0..l.screens.len() {
        for j in i + 1..l.screens.len() {
            if l.screens[i].id.device == l.screens[j].id.device {
                continue;
            }
            let (a, b) = (&l.screens[i].rect, &l.screens[j].rect);
            prop_assert!(!facing::overlaps(*a, *b, facing::tolerance(&[*a, *b])), "{a:?} {b:?}");
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// 指针永远不会跑出所有屏幕、不会进入离线设备的屏幕；穿越记录与终点一致。
    #[test]
    fn pointer_stays_on_screens(
        l in prop_oneof![arb_layout(), arb_logical()],
        owner_modes in prop::array::uniform3((0.1f64..5.0, any::<bool>())),
        offline in prop::sample::select(vec![None, Some(A), Some(B), Some(X)]),
        start in (0usize..7, 0.0f64..1.0, 0.0f64..1.0),
        moves in prop::collection::vec((-(400.0f64 / 686.0)..400.0, -(400.0f64 / 686.0)..400.0), 1..30),
    ) {
        let owners = [A, B, X].into_iter().zip(owner_modes).map(|(id, (gain, proportional))| (id, OwnerLayoutPreferences {
            device_gain: gain,
            speed_mode: if proportional { SpeedMode::ScreenProportional } else { SpeedMode::Physical },
            ..Default::default()
        })).collect();
        let t = l.topology_online(&owners, |d| Some(*d) != offline);
        let s = &t.screens()[start.0 % t.screens().len()];
        let mut pos = ScreenPos {
            screen: s.id.clone(),
            local: Point::new(s.local.x + start.1 * s.local.w, s.local.y + start.2 * s.local.h),
        };
        for d in moves {
            let r = t.step(&pos, d).unwrap();
            let sc = t.screen(&r.to.screen).unwrap();
            prop_assert!(sc.local.contains(r.to.local), "{r:?}");
            let p = plane(&t, &r.to);
            let inside = sc.rect.contains(p)
                || (p.x >= sc.rect.x && p.y >= sc.rect.y
                    && p.x <= sc.rect.right() + (1e-9 / 686.0) && p.y <= sc.rect.bottom() + (1e-9 / 686.0));
            prop_assert!(inside, "{:?} {:?}", p, sc.rect);
            match r.crossed.last() {
                Some(last) => prop_assert_eq!(last, &r.to.screen),
                None => prop_assert_eq!(&r.to.screen, &pos.screen),
            }
            for c in &r.crossed {
                prop_assert!(t.is_active(c));
                prop_assert!(Some(c.device) != offline);
            }
            prop_assert!(r.crossed.len() <= 4);
            pos = r.to;
        }
    }

    /// 两块屏左右相对（间隙任意，含轻微重叠）：在重叠区内水平移动，穿过后 y 不变、x 连续（跳过间隙），
    /// 原路返回回到起点。中间隔着离线设备的屏幕时也一样。
    #[test]
    fn crossing_is_continuous_and_reversible(
        wa in (200.0f64 / 686.0)..800.0, ha in (150.0f64 / 686.0)..500.0,
        wb in (200.0f64 / 686.0)..800.0, hb in (150.0f64 / 686.0)..500.0,
        fdy in 0.0f64..1.0, gap in -(4.0f64 / 686.0)..2000.0,
        upa in 0.5f64..4.0, upb in 0.5f64..4.0,
        fy in 0.05f64..0.95, back in (1.0f64 / 686.0)..150.0, fwd in (1.0f64 / 686.0)..150.0,
        blocker in any::<bool>(),
    ) {
        let ra = Rect::new(0.0, 0.0, wa, ha);
        // 沿边重叠 ≥ 45/686 reference units
        let dy = -hb + (45.0 / 686.0) + fdy * (ha + hb - (90.0 / 686.0));
        let rb = Rect::new(wa + gap, dy, wb, hb);
        let (lo, hi) = (ra.y.max(rb.y), ra.bottom().min(rb.bottom()));
        prop_assume!(hi - lo > (40.0 / 686.0));
        let mut screens = vec![screen(A, "a", ra, upa), screen(B, "b", rb, upb)];
        if blocker && gap > (10.0 / 686.0) {
            screens.push(screen(X, "x", Rect::new(wa + (1.0 / 686.0), -(1000.0 / 686.0), gap - (2.0 / 686.0), 3000.0 / 686.0), 2.0 * 686.0));
        }
        let t = layout(screens).topology_online(&Default::default(), |d| *d != X);
        let y = lo + (15.0 / 686.0) + fy * (hi - lo - (30.0 / 686.0));
        let back = back.min(wa - (1.0 / 686.0));
        let fwd = fwd.min(wb - (1.0 / 686.0));
        let start = at(&t, &sid(A, "a"), Point::new(wa - back, y));
        let r = t.step(&start, (back + fwd, 0.0)).unwrap();
        prop_assert_eq!(&r.to.screen, &sid(B, "b"));
        prop_assert!(!r.blocked_x && !r.blocked_y);
        let p = plane(&t, &r.to);
        prop_assert!((p.y - y).abs() < (1e-6 / 686.0));
        prop_assert!((p.x - (rb.x + fwd)).abs() < (1e-6 / 686.0), "{p:?}");
        let r2 = t.step(&r.to, (-(back + fwd), 0.0)).unwrap();
        prop_assert_eq!(&r2.to.screen, &sid(A, "a"));
        let q = plane(&t, &r2.to);
        prop_assert!((q.x - (wa - back)).abs() < (1e-5 / 686.0) && (q.y - y).abs() < (1e-6 / 686.0), "{q:?}");
    }

    /// 沿边没有重叠（只在角上相对或错开）：墙挡住，永远过不去。
    #[test]
    fn no_overlap_along_edge_blocks(
        gap in 0.0f64..500.0, below in any::<bool>(), extra in 0.0f64..300.0,
        moves in prop::collection::vec(-(500.0f64 / 686.0)..500.0, 1..20),
    ) {
        let dy = if below { (300.0 / 686.0) + extra } else { -(300.0 / 686.0) - extra };
        let t = layout(vec![
            screen(A, "a", Rect::new(0.0, 0.0, 500.0 / 686.0, 300.0 / 686.0), 2.0 * 686.0),
            screen(B, "b", Rect::new((500.0 / 686.0) + gap, dy, 500.0 / 686.0, 300.0 / 686.0), 2.0 * 686.0),
        ]).topology(&Default::default());
        let mut pos = at(&t, &sid(A, "a"), Point::new(250.0 / 686.0, 150.0 / 686.0));
        for m in moves {
            let r = t.step(&pos, (m, m * 0.3)).unwrap();
            prop_assert_eq!(&r.to.screen, &sid(A, "a"));
            pos = r.to;
        }
    }

    /// 每条边的切分覆盖整条边、按顺序、互不重叠；传送门的目标是另一块参与的屏幕，且不越过别的参与屏幕
    /// （间隙里没有沿边与相对范围重叠超过 `overlap_tolerance` 的参与屏幕）。
    #[test]
    fn segments_partition_edges(l in arb_layout(), offline in prop::sample::select(vec![None, Some(A), Some(X)])) {
        let t = l.topology_online(&Default::default(), |d| Some(*d) != offline);
        for e in t.edges() {
            let s = t.screen(&e.screen).unwrap();
            let (lo, hi) = if e.side.is_vertical() { (s.rect.y, s.rect.bottom()) } else { (s.rect.x, s.rect.right()) };
            prop_assert!(!e.segments.is_empty());
            prop_assert!((e.segments[0].from - lo).abs() < (1e-9 / 686.0));
            prop_assert!((e.segments.last().unwrap().to - hi).abs() < (1e-9 / 686.0));
            for w in e.segments.windows(2) {
                prop_assert!((w[0].to - w[1].from).abs() < (1e-9 / 686.0));
            }
            for g in &e.segments {
                prop_assert!(g.to > g.from);
                if let SegmentKind::Portal { to, .. } = &g.kind {
                    prop_assert!(to != &e.screen);
                    prop_assert!(t.is_active(to) && t.is_active(&e.screen));
                    let rb = t.screen(to).unwrap().rect;
                    let span = |r: &Rect| if e.side.is_vertical() { ((r.x, r.right()), (r.y, r.bottom())) } else { ((r.y, r.bottom()), (r.x, r.right())) };
                    let ((b0, b1), (bl, bh)) = span(&rb);
                    let far = match e.side { Side::Right | Side::Bottom => b0, _ => b1 };
                    let (g0, g1) = (e.at.min(far), e.at.max(far));
                    let (lo, hi) = (g.from.max(bl), g.to.min(bh));
                    for c in t.screens() {
                        if &c.id == to || c.id == e.screen || !t.is_active(&c.id) {
                            continue;
                        }
                        let ((c0, c1), (d0, d1)) = span(&c.rect);
                        let between = c1.min(g1) - c0.max(g0) > (1e-9 / 686.0);
                        let along = d1.min(hi) - d0.max(lo) > l.settings.overlap_tolerance + (1e-9 / 686.0);
                        prop_assert!(!(between && along), "{:?} {:?} 越过 {:?}", e.screen, to, c.id);
                    }
                }
            }
        }
    }

    /// 本机坐标与视平面互为逆映射。
    #[test]
    fn plane_local_inverse(l in arb_layout(), fx in 0.0f64..1.0, fy in 0.0f64..1.0) {
        let t = l.topology(&Default::default());
        for s in t.screens() {
            let local = Point::new(s.local.x + fx * s.local.w * 0.999, s.local.y + fy * s.local.h * 0.999);
            let plane = t.to_plane(&ScreenPos { screen: s.id.clone(), local }).unwrap();
            let back = t.from_plane(&s.id, plane).unwrap();
            prop_assert!((back.x - local.x).abs() < (1e-6 / 686.0) && (back.y - local.y).abs() < (1e-6 / 686.0));
        }
    }

    /// 规整：不重叠、尺寸不变、再规整一次不变（确定且稳定）。
    #[test]
    fn normalize_invariants(l in arb_layout()) {
        let mut n = l.clone();
        n.normalize();
        assert_no_overlap(&n)?;
        for (p, q) in l.screens.iter().zip(&n.screens) {
            prop_assert_eq!(p.rect.w, q.rect.w);
            prop_assert_eq!(p.rect.h, q.rect.h);
            prop_assert_eq!(&p.id, &q.id);
        }
        let mut again = n.clone();
        again.normalize();
        for (p, q) in n.screens.iter().zip(&again.screens) {
            prop_assert!((p.rect.x - q.rect.x).abs() < (1e-3 / 686.0) && (p.rect.y - q.rect.y).abs() < (1e-3 / 686.0),
                "{:?} → {:?}", p.rect, q.rect);
        }
        let mut twice = l.clone();
        twice.normalize();
        prop_assert_eq!(&twice, &n, "确定性");
    }

    /// 编辑：在逻辑布局上随意拖放屏幕或整台设备，结果仍不重叠、再规整不变，被拖的屏幕贴着别的屏幕。
    #[test]
    fn edits_keep_invariants(
        l in arb_logical(),
        ops in prop::collection::vec((0usize..7, -(1500.0f64 / 686.0)..1500.0, -(1000.0f64 / 686.0)..1000.0, any::<bool>()), 1..6),
    ) {
        let mut l = l;
        for (i, x, y, whole_device) in ops {
            let s = l.screens[i % l.screens.len()].clone();
            if whole_device {
                l.move_device(s.id.device, Point::new(x - s.rect.x, y - s.rect.y));
            } else {
                l.move_screen(&s.id, Point::new(x, y));

            }
            // Free drag previews retain their requested gaps; a committed edit
            // normalizes the complete layout, including untouched computers.
            l.normalize();
            assert_no_overlap(&l)?;
            let mut again = l.clone();
            again.normalize();
            for (p, q) in l.screens.iter().zip(&again.screens) {
                prop_assert!((p.rect.x - q.rect.x).abs() < (1e-3 / 686.0) && (p.rect.y - q.rect.y).abs() < (1e-3 / 686.0));
            }
        }
    }
}

#[test]
fn owner_device_exclusion_keeps_shared_geometry_and_geometry_settings() {
    let mut owners: BTreeMap<DeviceId, OwnerLayoutPreferences> = BTreeMap::new();
    let saved = layout(vec![
        screen(A, "a", Rect::new(0., 0., 200. / 686.0, 160. / 686.0), 5. * 686.0),
        screen(
            B,
            "b",
            Rect::new(210. / 686.0, 0., 200. / 686.0, 160. / 686.0),
            5. * 686.0,
        ),
    ]);
    let placements = saved.screens.clone();
    owners.entry(B).or_default().screen_shared = false;
    let mut json = serde_json::to_value(&saved.settings).unwrap();
    json["future_setting"] = serde_json::json!(true);
    assert_eq!(serde_json::from_value::<LayoutSettings>(json).unwrap(), saved.settings);
    assert_eq!(saved.screens, placements);
    assert_eq!(saved.view(&owners, |_| true).screens.len(), 2);
    assert!(!Topology::new(&saved, &owners).is_active(&sid(B, "b")));
    owners.entry(B).or_default().screen_shared = true;
    assert_eq!(saved.screens, placements);
}

#[test]
fn effective_exclusion_matches_removing_a_rectangle_for_every_existing_portal_rule() {
    for (x, y) in [
        ((210. / 686.0), 0.),
        ((630. / 686.0), 0.),
        (0., -(170. / 686.0)),
        (0., (170. / 686.0)),
    ] {
        for walls in [true, false] {
            let mut owners: BTreeMap<DeviceId, OwnerLayoutPreferences> = BTreeMap::new();
            let mut saved = layout(vec![
                screen(A, "a", Rect::new(0., 0., 200. / 686.0, 160. / 686.0), 2. * 686.0),
                screen(X, "x", Rect::new(x, y, 200. / 686.0, 160. / 686.0), 3. * 686.0),
                screen(
                    B,
                    "b",
                    Rect::new(420. / 686.0, 0., 200. / 686.0, 160. / 686.0),
                    5. * 686.0,
                ),
            ]);
            saved.settings.walls = walls;
            let before = saved.clone();
            owners.entry(X).or_default().screen_shared = false;
            let effective = saved.effective(&owners);
            assert_eq!(saved.screens, before.screens);
            assert_eq!(saved.view(&owners, |_| true).screens.len(), 3);
            assert_eq!(effective.screens().len(), 2);
            assert!(!effective.contains_device(X));
            let t = effective.topology();
            assert!(t.screen(&sid(X, "x")).is_none());
            assert!(
                t.to_plane(&ScreenPos {
                    screen: sid(X, "x"),
                    local: Point::new(1., 1.)
                })
                .is_none()
            );
            let mut removed = before.clone();
            removed.screens.retain(|s| s.id.device != X);
            assert_eq!(t.edges(), removed.topology(&Default::default()).edges());
            assert_eq!(t.screens(), removed.topology(&Default::default()).screens());
            // Target omission does not change the disabled device's source feel.
            assert_eq!(
                t.source_reference_per_native_unit(&sid(X, "x")),
                before
                    .topology(&Default::default())
                    .reference_per_native_unit(&sid(X, "x"))
            );
            assert_eq!(
                t.source_origin(X),
                Some(before.screen(&sid(X, "x")).unwrap().rect.center())
            );
            owners.entry(X).or_default().screen_shared = true;
            assert_eq!(saved.screens, before.screens);
            assert_eq!(
                saved.topology(&owners).edges(),
                before.topology(&Default::default()).edges()
            );
        }
    }
}

#[test]
fn source_redirection_is_explicit_and_not_inferred_from_missing_or_offline_geometry() {
    assert!(
        !Layout::default()
            .effective(&Default::default())
            .source_requires_target(A)
    );
    let mut owners: BTreeMap<DeviceId, OwnerLayoutPreferences> = BTreeMap::new();
    let saved = layout(vec![
        screen(A, "a", Rect::new(0., 0., 200. / 686.0, 160. / 686.0), 2. * 686.0),
        screen(
            B,
            "b",
            Rect::new(210. / 686.0, 0., 200. / 686.0, 160. / 686.0),
            2. * 686.0,
        ),
    ]);
    let offline = saved.topology_online(&Default::default(), |id| *id == A);
    assert!(!offline.contains_device(B));
    assert!(!offline.source_requires_target(B));
    assert!(!offline.source_requires_target(X));
    owners.entry(B).or_default().screen_shared = false;
    let effective = saved.effective(&owners);
    assert!(!effective.contains_device(B));
    assert!(effective.source_requires_target(B));
    assert!(effective.topology().source_requires_target(B));
    assert_eq!(saved.screens.len(), 2);
    owners.entry(B).or_default().screen_shared = true;
    assert!(!saved.effective(&owners).source_requires_target(B));
}

#[test]
fn exclusion_uses_existing_unbounded_gap_and_overlap_rules_without_new_walls() {
    let mut owners: BTreeMap<DeviceId, OwnerLayoutPreferences> = BTreeMap::new();
    let mut saved = layout(vec![
        screen(A, "a", Rect::new(0., 0., 200. / 686.0, 160. / 686.0), 1. * 686.0),
        screen(
            X,
            "x",
            Rect::new(210. / 686.0, 0., 200. / 686.0, 160. / 686.0),
            1. * 686.0,
        ),
        screen(
            B,
            "b",
            Rect::new(10000. / 686.0, 0., 200. / 686.0, 160. / 686.0),
            1. * 686.0,
        ),
    ]);
    owners.entry(X).or_default().screen_shared = false;
    assert!(
        portals(&saved.topology(&owners), &sid(A, "a"), Side::Right)
            .iter()
            .any(|p| p.2.is_some())
    );
    saved.screens[2].rect.y = 170. / 686.0; // No along-edge overlap; only a corner/diagonal.
    assert!(
        portals(&saved.topology(&owners), &sid(A, "a"), Side::Right)
            .iter()
            .all(|p| p.2.is_none())
    );
}

#[test]
fn owner_gain_modes_and_screen_enable_never_enter_shared_geometry() {
    let raw = layout(vec![
        screen(A, "a", Rect::new(0., 0., 0.8, 0.6), 1000.),
        screen(B, "b", Rect::new(0.8, 0., 1.6, 1.2), 1000.),
    ]);
    let before = serde_json::to_value(&raw).unwrap();
    let mut owners = BTreeMap::from([
        (
            A,
            OwnerLayoutPreferences {
                device_gain: 2.,
                ..Default::default()
            },
        ),
        (
            B,
            OwnerLayoutPreferences {
                speed_mode: SpeedMode::ScreenProportional,
                ..Default::default()
            },
        ),
    ]);
    owners.get_mut(&A).unwrap().screens.insert(
        "a".into(),
        ScreenPreference {
            remote_gain: 1.0,
            gain: 3.,
            enabled: true,
        },
    );
    let topology = raw.topology(&owners);
    assert_eq!(topology.plane_gain(&sid(A, "a")), Some(6.));
    assert_eq!(topology.plane_gain(&sid(B, "b")), Some(2.));
    owners.get_mut(&A).unwrap().screen_shared = false;
    let excluded = raw.topology(&owners);
    assert!(!excluded.contains_device(A));
    assert_eq!(
        excluded.source_reference_per_native_unit(&sid(A, "a")),
        topology.reference_per_native_unit(&sid(A, "a"))
    );
    assert_eq!(serde_json::to_value(&raw).unwrap(), before);
    assert!(before["screens"][0].get("gain").is_none());
    assert!(before["screens"][0].get("enabled").is_none());
    assert_eq!(before["settings"].as_object().unwrap().len(), 6);
}

#[test]
fn native_group_keeps_one_scale_and_native_origin_on_live_rearrange() {
    let device = DeviceId([31; 32]);
    let mut a = DisplayInfo {
        key: "a".into(),
        name: "a".into(),
        bounds: Rect::new(0.0, 0.0, 1000.0, 600.0),
        pixel_width: 2000,
        pixel_height: 1200,
        reference_extent: Some(Size::new(1.0, 0.6)),
        rotation_deg: 0,
        is_primary: true,
        is_builtin: false,
        is_virtual: false,
    };
    let mut b = a.clone();
    b.key = "b".into();
    b.is_primary = false;
    b.bounds = Rect::new(-800.0, 100.0, 800.0, 500.0);
    b.reference_extent = Some(Size::new(2.0, 1.25));
    let mut layout = Layout::auto(&[(device, vec![a.clone(), b.clone()])]);
    let id = ScreenId::new(device, "a");
    let old = layout.screen(&id).unwrap().clone();
    let peer = layout.screen(&ScreenId::new(device, "b")).unwrap();
    let k = old.rect.w / old.local.w;
    assert!((peer.rect.w / peer.local.w - k).abs() < 1e-12);
    assert!((peer.rect.x - old.rect.x + 800.0 * k).abs() < 1e-12);
    b.bounds = Rect::new(1000.0, -200.0, 800.0, 500.0);
    layout.sync_device(device, &[a.clone(), b.clone()]);
    assert_eq!(layout.screen(&id).unwrap().rect, old.rect);
    let moved = layout.screen(&ScreenId::new(device, "b")).unwrap();
    assert!((moved.rect.y - old.rect.y + 200.0 * k).abs() < 1e-12);
    a.bounds.w = 1200.0;
    layout.sync_device(device, &[a, b]);
    assert!((layout.screen(&id).unwrap().rect.w - 1200.0 * k).abs() < 1e-12);
}

#[test]
fn moving_and_resizing_a_screen_transform_its_entire_computer() {
    let device = DeviceId([32; 32]);
    let mut layout = Layout::default();
    for (name, x) in [("a", 0.0), ("b", 1000.0)] {
        layout.screens.push(ScreenPlacement {
            id: ScreenId::new(device, name),
            name: name.into(),
            local: Rect::new(x, 0.0, 1000.0, 600.0),
            rect: Rect::new(x / 1000.0, 0.0, 1.0, 0.6),
            size_source: SizeSource::Reported,
            primary: name == "a",
            builtin: false,
        });
    }
    let id = ScreenId::new(device, "b");
    layout.move_screen(&id, Point::new(3.0, 2.0));
    assert_eq!(layout.screens[0].rect.x, 2.0);
    assert_eq!(layout.screens[0].rect.y, 2.0);
    layout.resize_screen(&id, Size::new(2.0, 1.2));
    assert_eq!(layout.screens[0].rect.w, 2.0);
    assert_eq!(layout.screens[1].rect.x - layout.screens[0].rect.x, 2.0);
}

#[test]
fn native_seams_ignore_cross_computer_wall_extension_and_minimum_portal() {
    let mut layout = layout(vec![
        screen(A, "a", Rect::new(0.0, 0.0, 1.0, 0.6), 1000.0),
        screen(A, "b", Rect::new(1.0, 0.5999, 0.5, 0.4), 1000.0),
    ]);
    layout.settings.walls = false;
    layout.settings.endpoint_reach = 8.0 / 686.0;
    let topology = layout.topology(&Default::default());
    let valid = at(&topology, &sid(A, "a"), Point::new(0.999, 0.59995));
    assert_eq!(
        topology.first_portal(&valid, (0.002, 0.0)).unwrap().to.screen,
        sid(A, "b")
    );
    let wall = at(&topology, &sid(A, "a"), Point::new(0.999, 0.59));
    assert!(topology.first_portal(&wall, (0.002, 0.0)).is_none());
    assert!(topology.trigger_zones().is_empty());
}
