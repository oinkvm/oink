//! 光标定位拖尾：在输入源上识别快速画圈，告诉指针所在的设备画拖尾。
//!
//! - **源端**（物理键鼠所在设备）：每个本机 `PointerDelta` 按手部 plane 喂给 [`CircleDetector`]，不论焦点
//!   在哪台设备。触发后把「画拖尾」交给当前焦点机：焦点机是本机就记在本机，否则发
//!   `ControlMsg::CursorTrail{active: true}`；焦点换到另一台设备时给旧的发 `false`、给新的发 `true`，
//!   所以画圈途中跨设备不清零，拖尾跟着指针走。结束（停住、按下按钮、游戏模式、关闭设置）
//!   时发 `false`。
//! - **焦点机**：记下要它画拖尾的源（含本机）；有源、焦点在本机、不在游戏模式、本机光标没有按着的按钮、
//!   本机设置开着时输出 `Output::CursorTrail{active: true}`，由服务进程按本机光标的轨迹绘制。
//!   源断线时去掉它。

use super::*;
use crate::circle::CircleDetector;

#[derive(Debug, Default)]
pub(super) struct TrailState {
    /// 源端：画圈识别。
    detector: CircleDetector,
    /// 源端：已让哪台设备画拖尾（本机或远端焦点机）。
    target: Option<DeviceId>,
    /// 焦点机：要本机画拖尾的源。
    sources: BTreeSet<DeviceId>,
    /// 焦点机：已输出的绘制状态。
    drawing: bool,
    pending: Option<PendingTrail>,
    /// 焦点机：常驻显示（瞄准小游戏进行中，[`Router::set_trail_pinned`]）。
    pinned: bool,
}

#[derive(Debug)]
struct PendingTrail {
    from: DeviceId,
    epoch: u64,
    sources: Vec<DeviceId>,
    state: crate::control::TrailContinuation,
    received: Micros,
}

impl Router {
    /// 本机此刻是否在画拖尾（最近一次 `Output::CursorTrail` 的值）。
    pub fn cursor_trail_drawing(&self) -> bool {
        self.trail.drawing
    }

    /// Keep the trail visible while the local pointer participates in an aim session; no circle required.
    /// 触发，不受本机拖尾开关、「按钮按着时不画」的限制（游戏里每次点击都按着按钮）；指针不在本机、
    /// 指针锁定的游戏模式下仍然不画。跨设备的连续交接照常（两端都常驻时不需要源的触发）。
    /// 关掉后回到原来的规则与设置。
    pub fn set_trail_pinned(&mut self, pinned: bool) {
        self.trail.pinned = pinned;
    }

    pub fn trail_pinned(&self) -> bool {
        self.trail.pinned
    }

    /// 源端画圈识别的当前圈数（诊断、测试用）。
    pub fn cursor_trail_turns(&self) -> f64 {
        self.trail.detector.turns()
    }

    /// 焦点在本机时本机光标的位置（本机全局坐标）；不分配。
    pub fn local_pointer(&self) -> Option<Point> {
        (self.holder == self.me()).then(|| self.pointer.as_ref().map(|p| p.local))?
    }

    /// 本机光标此刻是否有按钮按着：本机外设按下且交给本机，或为某个源注入。
    pub fn pointer_buttons_held(&self) -> bool {
        self.buttons
            .values()
            .any(|t| matches!(t, Target::Local | Target::LocalInjected))
            || self.button_refs.values().any(|&n| n > 0)
    }

    /// 源端：本机外设的一次移动（本机单位）。在游戏模式判断之前调用。
    pub(super) fn trail_motion(&mut self, dx: f64, dy: f64, now: Micros) {
        if let Some(why) = self.trail_source_blocked() {
            self.trail_reset(why);
            return;
        }
        let k = if self.holder == self.me() {
            self.pointer_reference_per_native_unit()
        } else {
            self.reference_per_native_unit()
        };
        self.trail.detector.motion(dx * k, dy * k, now);
    }

    /// 源端能否识别：设置开着、不在游戏模式、本机外设没有按着按钮（拖动窗口、画图时画圈不触发）。
    /// 不能时给出原因。
    fn trail_source_blocked(&self) -> Option<&'static str> {
        if !self.cfg.settings.cursor_trail {
            Some("setting off")
        } else if self.game_mode().active {
            Some("game mode")
        } else if self.buttons.values().any(|t| *t != Target::Dropped) {
            Some("button held")
        } else {
            None
        }
    }

    /// 源端清零识别；已触发时记下原因（识别器自己的结束原因见 `circle gesture ended`）。
    fn trail_reset(&mut self, why: &'static str) {
        if self.trail.detector.engaged() {
            tracing::debug!(reason = why, "cursor trail source stopped");
        }
        self.trail.detector.reset();
    }

    pub(super) fn trail_deadline(&self) -> Option<Micros> {
        self.trail.detector.deadline()
    }

    pub(super) fn trail_tick(&mut self, now: Micros) {
        self.trail.detector.tick(now);
    }

    /// 焦点机：收到源的通知。
    pub(super) fn trail_msg(&mut self, from: DeviceId, active: bool) {
        if active {
            self.trail.sources.insert(from);
        } else {
            self.trail.sources.remove(&from);
        }
    }

    pub(super) fn trail_peer_lost(&mut self, peer: DeviceId) {
        self.trail.sources.remove(&peer);
        if self.trail.pending.as_ref().is_some_and(|p| p.from == peer) {
            self.trail.pending = None;
        }
        if self.trail.target == Some(peer) {
            self.trail.target = None;
        }
    }

    /// 每个入口处理完后调用：源端按识别结果与当前焦点机更新通知；焦点机更新绘制状态。
    pub(super) fn trail_sync(&mut self) {
        let me = self.me();
        if self.trail.detector.engaged()
            && let Some(why) = self.trail_source_blocked()
        {
            self.trail_reset(why);
        }
        let want = self.trail.detector.engaged().then_some(self.holder);
        if want != self.trail.target {
            if let Some(old) = self.trail.target.take() {
                if old == me {
                    self.trail.sources.remove(&me);
                } else {
                    self.send(old, Message::Control(ControlMsg::CursorTrail { active: false }));
                }
            }
            if let Some(new) = want {
                if new == me {
                    self.trail.sources.insert(me);
                } else {
                    self.send(new, Message::Control(ControlMsg::CursorTrail { active: true }));
                }
            }
            self.trail.target = want;
        }
        let draw = self.holder == me
            && !self.game_mode().active
            && (self.trail.pinned
                || (self.cfg.settings.cursor_trail && !self.trail.sources.is_empty() && !self.pointer_buttons_held()));
        if draw != self.trail.drawing {
            self.trail.drawing = draw;
            tracing::debug!(draw, sources = self.trail.sources.len(), "cursor trail");
            self.out.push(Output::CursorTrail { active: draw });
        }
    }
}

use crate::control::{MAX_TRAIL_POINTS, TrailContinuation, TrailPoint};

/// Derive the affine layout scale without using from_plane (production topology
/// clamps it, which would collapse the off-screen half of an incoming ribbon).
fn plane_scale(layout: &dyn LayoutQuery, screen: &ScreenId) -> Option<(f64, f64)> {
    let o = layout.to_plane(&ScreenPos::new(screen.clone(), Point::new(0., 0.)))?;
    let p = layout.to_plane(&ScreenPos::new(screen.clone(), Point::new(1., 1.)))?;
    let k = (p.x - o.x, p.y - o.y);
    (k.0 > 0. && k.1 > 0.).then_some(k)
}

impl Router {
    /// Renderer snapshot probe; no platform/network work is performed by routing.
    pub fn set_trail_probe(&mut self, probe: Arc<dyn Fn(Point) -> Option<TrailContinuation> + Send + Sync>) {
        self.trail_probe = Some(probe);
    }

    pub(super) fn trail_handoff(&mut self, dst: &ScreenPos, epoch: u64, head: Option<Point>) {
        if !self.trail.drawing {
            return;
        }
        let Some(exit) = self.pointer.clone() else { return };
        let Some(head) = head else { return };
        self.out.push(Output::CursorTrailDepart { exit: head });
        if self.trail.sources.len() > 16 {
            return;
        }
        let Some(mut state) = self.trail_probe.as_ref().and_then(|p| p(head)) else {
            return;
        };
        let Some((kx, ky)) = plane_scale(self.layout.as_ref(), &exit.screen) else {
            return;
        };
        // Gap travel samples already include the physical journey and its real
        // duration. Keep their absolute plane offsets relative to the arrival.
        let anchor = if self.gap.is_some() {
            let Some(dst_plane) = self.layout.to_plane(dst) else {
                return;
            };
            let Some(src_plane) = self.layout.to_plane(&exit) else {
                return;
            };
            Point::new(
                exit.local.x + (dst_plane.x - src_plane.x) / kx,
                exit.local.y + (dst_plane.y - src_plane.y) / ky,
            )
        } else {
            head
        };
        if let Some(d) = self.diagnostics.as_ref().filter(|d| d.enabled()) {
            d.trail(
                super::TrailStage::Source,
                epoch,
                &exit.screen,
                anchor,
                Point::new(kx, ky),
                &state,
            );
        }
        state.points.truncate(MAX_TRAIL_POINTS - 1);
        for p in &mut state.points {
            p.pos = Point::new((p.pos.x - anchor.x) * kx, (p.pos.y - anchor.y) * ky);
        }
        // Ensure the arrival endpoint is the final point, even between render frames.
        state.points.push(TrailPoint {
            age_secs: 0.,
            pos: Point::new(0., 0.),
        });
        state.heading = Point::new(state.heading.x * kx, state.heading.y * ky);
        state.native_units_per_reference *= (kx * ky).sqrt();
        if let Some(d) = self.diagnostics.as_ref().filter(|d| d.enabled()) {
            d.trail(
                super::TrailStage::Converted,
                epoch,
                &dst.screen,
                anchor,
                Point::new(kx, ky),
                &state,
            );
        }
        self.diagnostic_log(line!(), "cursor trail handoff", epoch);
        tracing::info!(
            epoch,
            points = state.points.len(),
            size_scale = state.size_scale,
            plane_units_per_reference = state.native_units_per_reference,
            "cursor trail handoff"
        );
        if dst.screen.device == self.me() {
            // Empty-space travel can return to a different local display whose
            // OS coordinates/scale differ from the leaving display's extension.
            self.pointer = Some(dst.clone());
            self.trail_enter(epoch, dst, self.trail.sources.iter().copied().collect(), state);
            return;
        }
        self.send(
            dst.screen.device,
            Message::Control(ControlMsg::CursorTrailHandoff {
                epoch,
                sources: self.trail.sources.iter().copied().collect(),
                state,
            }),
        );
    }

    fn trail_enter(&mut self, epoch: u64, at: &ScreenPos, sources: Vec<DeviceId>, mut state: TrailContinuation) {
        if epoch != self.epoch
            || self.holder != self.me()
            || !state.valid()
            || sources.len() > 16
            || self.game_mode().active
            || (!self.trail.pinned
                && (sources.is_empty() || !self.cfg.settings.cursor_trail || self.pointer_buttons_held()))
        {
            return;
        }
        let entry = at.local;
        let Some((kx, ky)) = plane_scale(self.layout.as_ref(), &at.screen) else {
            return;
        };
        for p in &mut state.points {
            p.pos = Point::new(entry.x + p.pos.x / kx, entry.y + p.pos.y / ky);
        }
        state.heading = Point::new(state.heading.x / kx, state.heading.y / ky);
        state.native_units_per_reference /= (kx * ky).sqrt();
        self.trail.sources.extend(sources);
        self.out.push(match self.trail_clip_bounds() {
            Some(clip) => Output::CursorTrailResumeClipped { state, clip },
            None => Output::CursorTrailResume(state),
        });
    }
}

impl Router {
    pub(super) fn trail_receive(&mut self, from: DeviceId, msg: ControlMsg, now: Micros) {
        let ControlMsg::CursorTrailHandoff { epoch, sources, state } = msg else {
            return;
        };
        if epoch < self.epoch || sources.len() > 16 || !state.valid() {
            return;
        }
        self.trail.pending = Some(PendingTrail {
            from,
            epoch,
            sources,
            state,
            received: now,
        });
    }

    pub(super) fn trail_focus_entered(&mut self, from: DeviceId, epoch: u64, at: &ScreenPos, now: Micros) {
        let Some(mut p) = self.trail.pending.take() else { return };
        if p.from != from || p.epoch != epoch {
            return;
        }
        p.state.age(now.saturating_sub(p.received) as f64 / 1e6);
        self.trail_enter(epoch, at, p.sources, p.state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_layout::TopologyLayout;
    use oink_display::layout::{Layout, ScreenPlacement, SizeSource};

    #[test]
    fn handoff_maps_physical_offsets_across_gaps_without_clamping_or_hand_gain() {
        let a = DeviceId([1; 32]);
        let b = DeviceId([2; 32]);
        for vertical in [false, true] {
            for reverse in [false, true] {
                let make = |device, rect| ScreenPlacement {
                    id: oink_display::layout::ScreenId::new(device, "screen"),
                    name: "screen".into(),
                    local: Rect::new(0., 0., 1500., 1000.),
                    rect,
                    size_source: SizeSource::Manual,
                    primary: true,
                    builtin: true,
                };
                let rb = if vertical {
                    Rect::new(0., 268., 201., 134.)
                } else {
                    Rect::new(368., 0., 201., 134.)
                };
                let layout = Arc::new(TopologyLayout::from_layout(
                    &Layout {
                        screens: vec![make(a, Rect::new(0., 0., 300., 200.)), make(b, rb)],
                        ..Layout::default()
                    },
                    &std::collections::BTreeMap::from([
                        (
                            a,
                            oink_display::layout::OwnerLayoutPreferences {
                                device_gain: 2.3,
                                ..Default::default()
                            },
                        ),
                        (
                            b,
                            oink_display::layout::OwnerLayoutPreferences {
                                device_gain: 0.67,
                                ..Default::default()
                            },
                        ),
                    ]),
                ));
                let (src, dst, scale_src, scale_dst) = if reverse {
                    (b, a, 0.134, 0.2)
                } else {
                    (a, b, 0.2, 0.134)
                };
                let edge = if reverse {
                    0.
                } else if vertical {
                    1000. - 1e-6
                } else {
                    1500. - 1e-6
                };
                let exit = ScreenPos::new(
                    ScreenId::new(src, "screen"),
                    if vertical {
                        Point::new(500., edge)
                    } else {
                        Point::new(edge, 500.)
                    },
                );
                let direction = if reverse { -1. } else { 1. };
                let delta = if vertical { (0., direction) } else { (direction, 0.) };
                let entry = layout.exit_through_edge(&exit, delta).unwrap();
                assert_eq!(entry.screen.device, dst);
                let old = if vertical {
                    Point::new(exit.local.x - 5., exit.local.y - direction * 60.)
                } else {
                    Point::new(exit.local.x - direction * 60., exit.local.y - 5.)
                };
                let state = TrailContinuation {
                    shown_secs: 2.,
                    size_scale: 0.81,
                    native_units_per_reference: 5.,
                    emit_carry: 0.75,
                    heading: Point::new(delta.0, delta.1),
                    points: vec![
                        TrailPoint {
                            age_secs: 0.15,
                            pos: old,
                        },
                        TrailPoint {
                            age_secs: 0.,
                            pos: exit.local,
                        },
                    ],
                };
                let mut sender = Router::new(
                    RouterConfig::new(
                        oink_foundation::session::Boot {
                            device_id: src,
                            nonce: [src.0[0]; 16],
                        },
                        "src",
                        Os::MacOS,
                    ),
                    layout.clone(),
                );
                sender.test_connect(dst, 0);
                sender.pointer = Some(exit.clone());
                sender.trail.drawing = true;
                sender.trail.sources.insert(src);
                sender.set_trail_probe(Arc::new(move |_| Some(state.clone())));
                sender.trail_handoff(&entry, 3, Some(exit.local));
                let wire = sender
                    .out
                    .iter()
                    .find_map(|o| match o {
                        Output::Send {
                            msg: Message::Control(ControlMsg::CursorTrailHandoff { state, .. }),
                            ..
                        } => Some(state.clone()),
                        _ => None,
                    })
                    .unwrap();
                let mut receiver = Router::new(
                    RouterConfig::new(
                        oink_foundation::session::Boot {
                            device_id: dst,
                            nonce: [dst.0[0]; 16],
                        },
                        "dst",
                        Os::Windows,
                    ),
                    layout,
                );
                receiver.epoch = 3;
                receiver.trail_enter(3, &entry, vec![src], wire);
                let state = receiver
                    .out
                    .iter()
                    .find_map(|o| match o {
                        Output::CursorTrailResume(s) => Some(s),
                        _ => None,
                    })
                    .unwrap();
                let got = state.points[0].pos;
                assert!((got.x - (entry.local.x + (old.x - exit.local.x) * scale_src / scale_dst)).abs() < 1e-8);
                assert!((got.y - (entry.local.y + (old.y - exit.local.y) * scale_src / scale_dst)).abs() < 1e-8);
                assert!((state.native_units_per_reference - 5. * scale_src / scale_dst).abs() < 1e-8);
                assert_eq!(state.size_scale, 0.81);
                assert_eq!(state.points.last().unwrap().pos, entry.local);
                let outside = if vertical { got.y } else { got.x };
                assert!(if reverse {
                    outside > if vertical { 1000. } else { 1500. }
                } else {
                    outside < 0.
                });
                // 68/686 reference units gap is absent from every relative vector; no synthetic travel time.
                assert_eq!(state.points[0].age_secs, 0.15);
            }
        }
    }
}
