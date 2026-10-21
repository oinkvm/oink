//! Map Mac gestures and system function keys to Windows shortcuts.
//! The pure state machine can be tested on any platform; the Windows backend executes its output.
//!
//! | 来自 Mac | Windows |
//! |---|---|
//! | 上轻扫 Ended 且绝对进度峰值 ≥ 0.05 | 桌面前台时 Win+D 恢复窗口，否则任务视图 Win+Tab |
//! | 下轻扫 Ended 且绝对进度峰值 ≥ 0.05 | 任务视图前台 Esc；桌面前台不操作；其他 Win+D 显示桌面 |
//! | Dock Pinch（同样 Ended + 峰值门限） | 收拢且 Start/搜索未开 → Win；张开且已开 → Esc；其余不操作 |
//! | 左右轻扫 Ended 且绝对进度峰值 ≥ 0.05 | 切换虚拟桌面 Ctrl+Win+← / → |
//! | 捏合缩放 | Ctrl+滚轮（放大量每累计 [`MAGNIFY_PER_NOTCH`] 一格） |
//! | 聚焦搜索键 | 浏览器搜索键（`VK_BROWSER_SEARCH`，Windows 打开搜索；见 `windows/keymap.rs`） |
//! | 听写键 | 语音输入 Win+H |
//! | 启动台 / Apps 键 | 开始菜单（单按 Win） |
//! | 其余（旋转、智能缩放、勿扰键） | 丢弃（debug 日志） |

use oink_foundation::input::{Gesture, GesturePhase, KeyCode, SwipeAxis};

/// 系统的 Ended / Cancelled 已携带提交决定；这个绝对进度峰值下限只过滤手势碎片。
pub const SWIPE_PROGRESS_FLOOR: f64 = 0.05;
/// 捏合放大量每累计这么多发一格 Ctrl+滚轮（Windows 应用一格通常缩放 10%）。
pub const MAGNIFY_PER_NOTCH: f64 = 0.1;

/// Dock 轻扫的线上方向约定（Pinch 在 macOS 截获端按字段 136 归一化，不是原始进度）：竖直进度为负 = 向上（调度中心），为正 = 向下
/// （App Exposé，收起调度中心）；捏合进度为负 = 捏合（打开 Apps / 启动台），为正 = 张开（显示桌面）。
/// 左右：进度为正 = 手指向左、切到右边的桌面（**待真实触摸板记录核对**）。
pub const HORIZONTAL_POSITIVE_GOES_RIGHT: bool = true;

/// Windows 上执行的动作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WinAction {
    /// 依次按下这些键，再倒序抬起（快捷键）。
    Shortcut(Vec<KeyCode>),
    /// 按住 Ctrl 滚动若干格（正 = 向上 = 放大）。
    CtrlWheel(i32),
}

const WIN: KeyCode = KeyCode::LEFT_GUI;
const CTRL: KeyCode = KeyCode::LEFT_CTRL;
const TAB: KeyCode = KeyCode::keyboard(0x2B);
const ESC: KeyCode = KeyCode::keyboard(0x29);
const LEFT: KeyCode = KeyCode::keyboard(0x50);
const RIGHT: KeyCode = KeyCode::keyboard(0x4F);
const D: KeyCode = KeyCode::keyboard(0x07);
const H: KeyCode = KeyCode::keyboard(0x0B);

/// Mac 的系统功能键（`keymap` 里新款键盘的键）在 Windows 上的快捷键。`None`：没有自然对应（丢弃）或不是这类键。
pub fn key_shortcut(code: KeyCode) -> Option<Vec<KeyCode>> {
    Some(match (code.page(), code.usage()) {
        (0x0C, 0x029F) => vec![WIN, TAB], // 调度中心 → 任务视图
        (0x0C, 0x00CF) => vec![WIN, H],   // 听写 → 语音输入
        (0x0C, 0x02A2) => vec![WIN],      // 启动台 / Apps → 开始菜单
        _ => return None,
    })
}

/// Mac 能做、Windows 做不了的键：接收端直接丢弃（debug 日志，不报错）。
pub fn key_dropped(code: KeyCode) -> bool {
    matches!(
        (code.page(), code.usage()),
        (0x01, 0x009B) // 勿扰模式
    )
}

/// One decision-time foreground sample supplied only by the injection backend.
/// Query failures use the default (none of these Shell surfaces is showing).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ForegroundState {
    pub task_view_showing: bool,
    pub desktop_showing: bool,
    pub start_open: bool,
}

/// 手势 → Windows 动作的状态机（每个接收端一个）。
#[derive(Debug, Default)]
pub struct GestureMapper {
    /// 进行中的 Dock 轻扫：种类与累计进度的绝对值峰值。
    swipe: Option<(SwipeAxis, f64)>,
    /// 捏合放大量还没换成滚轮格的部分。
    magnify_rem: f64,
    trace: crate::gesture_trace::Tracker,
    /// Foreground sample at this frame's committed swipe; None means not queried.
    task_view_showing: Option<bool>,
    desktop_showing: Option<bool>,
    start_open: Option<bool>,
}

impl GestureMapper {
    pub fn map(&mut self, g: &Gesture) -> Vec<WinAction> {
        self.map_with_foreground_query(g, |_| ForegroundState::default())
    }
    /// The Windows injection caller supplies a desktop query, evaluated only after
    /// Ended + the progress floor. Capture/routing and pure callers make no syscalls.
    pub fn map_with_foreground_query(
        &mut self,
        g: &Gesture,
        query: impl FnOnce(SwipeAxis) -> ForegroundState,
    ) -> Vec<WinAction> {
        let actions = self.map_actions_with_foreground_query(g, query);
        if crate::native_lifecycle::lifecycle_enabled() {
            let stamp = crate::native_lifecycle::capture_stamp();
            let source = crate::gesture_trace::source();
            if let Some(summary) = self.observe(
                g,
                &actions,
                crate::native_lifecycle::gesture_time(stamp),
                crate::native_lifecycle::lifecycle_generation(),
                source,
            ) {
                crate::native_lifecycle::emit_gesture(
                    stamp,
                    crate::gesture_trace::Observation {
                        stage: crate::gesture_trace::Stage::WindowsMapper,
                        source,
                        summary,
                    },
                );
            }
        }
        actions
    }
    // Observations consume the returned actions.
    #[cfg(all(test, debug_assertions))]
    fn map_actions(&mut self, g: &Gesture) -> Vec<WinAction> {
        self.map_actions_with_foreground_query(g, |_| ForegroundState::default())
    }
    fn map_actions_with_foreground_query(
        &mut self,
        g: &Gesture,
        query: impl FnOnce(SwipeAxis) -> ForegroundState,
    ) -> Vec<WinAction> {
        self.task_view_showing = None;
        self.desktop_showing = None;
        self.start_open = None;
        match *g {
            Gesture::DockSwipe {
                axis, phase, progress, ..
            } => match phase {
                GesturePhase::Began => {
                    self.swipe = Some((axis, progress.abs()));
                    Vec::new()
                }
                GesturePhase::Changed => {
                    self.swipe = Some((axis, self.swipe_peak(axis, progress)));
                    Vec::new()
                }
                GesturePhase::Cancelled => {
                    self.swipe = None;
                    Vec::new()
                }
                GesturePhase::Ended => {
                    // 结束帧的符号决定方向；峰值只过滤碎片。中途接管时使用已见到的进度。
                    let peak_abs_progress = self.swipe_peak(axis, progress);
                    self.swipe = None;
                    if peak_abs_progress >= SWIPE_PROGRESS_FLOOR {
                        let foreground = query(axis);
                        if axis == SwipeAxis::Pinch {
                            self.start_open = Some(foreground.start_open);
                        } else {
                            self.task_view_showing = Some(foreground.task_view_showing);
                            self.desktop_showing = Some(foreground.desktop_showing);
                        }
                    }
                    swipe_action(axis, progress, peak_abs_progress, self.foreground())
                        .into_iter()
                        .collect()
                }
            },
            Gesture::Magnify { phase, amount } => {
                if phase == GesturePhase::Began {
                    self.magnify_rem = 0.0;
                }
                self.magnify_rem += amount;
                let notches = (self.magnify_rem / MAGNIFY_PER_NOTCH).trunc();
                self.magnify_rem -= notches * MAGNIFY_PER_NOTCH;
                if phase.is_end() {
                    self.magnify_rem = 0.0;
                }
                if notches == 0.0 {
                    Vec::new()
                } else {
                    vec![WinAction::CtrlWheel(notches as i32)]
                }
            }
            Gesture::Rotate { .. } | Gesture::SmartMagnify => Vec::new(),
        }
    }
    fn foreground(&self) -> ForegroundState {
        ForegroundState {
            task_view_showing: self.task_view_showing.unwrap_or(false),
            desktop_showing: self.desktop_showing.unwrap_or(false),
            start_open: self.start_open.unwrap_or(false),
        }
    }
    fn swipe_peak(&self, axis: SwipeAxis, progress: f64) -> f64 {
        self.swipe
            .filter(|(active_axis, _)| *active_axis == axis)
            .map_or(progress.abs(), |(_, peak)| peak.max(progress.abs()))
    }
    fn observe(
        &mut self,
        g: &Gesture,
        actions: &[WinAction],
        at_us: u64,
        generation: u64,
        source: Option<oink_foundation::DeviceId>,
    ) -> Option<crate::gesture_trace::Summary> {
        use crate::gesture_trace::{Decision, Outcome};
        let (decision, threshold) = match *g {
            Gesture::DockSwipe {
                axis, phase, progress, ..
            } => (
                if phase == GesturePhase::Cancelled {
                    Decision::Cancelled
                } else if phase != GesturePhase::Ended {
                    Decision::Pending
                } else if self.task_view_showing.is_none() && self.start_open.is_none() {
                    Decision::BelowThreshold
                } else {
                    // Sampling proves the mapper passed its peak gate, even if
                    // recording began later and its tracker missed that peak.
                    swipe_decision(axis, progress, self.foreground())
                },
                Some(SWIPE_PROGRESS_FLOOR),
            ),
            Gesture::Magnify { phase, .. } => (
                if phase == GesturePhase::Cancelled {
                    Decision::Cancelled
                } else {
                    Decision::CtrlWheel
                },
                Some(MAGNIFY_PER_NOTCH),
            ),
            _ => return None,
        };
        let notches = actions
            .iter()
            .filter_map(|a| match a {
                WinAction::CtrlWheel(n) => Some(*n),
                _ => None,
            })
            .fold(0i32, i32::saturating_add);
        let mut row = self.trace.observe(
            *g,
            at_us,
            generation,
            source,
            Outcome {
                decision,
                threshold,
                notches,
            },
        );
        if let Some(row) = &mut row {
            row.task_view_showing = self.task_view_showing;
            row.desktop_showing = self.desktop_showing;
            row.start_open = self.start_open;
        }
        // Cancellation does not undo Ctrl+wheel actions already generated, and the
        // existing mapper may generate more from the final Cancelled amount.
        if matches!(g, Gesture::Magnify { .. })
            && let Some(row) = &mut row
        {
            row.decision = if row.ctrl_wheel_notches != 0 {
                Decision::CtrlWheel
            } else if row.phase == GesturePhase::Cancelled {
                Decision::Cancelled
            } else if row.phase == GesturePhase::Began {
                Decision::Pending
            } else {
                Decision::BelowThreshold
            };
        }
        row
    }
}

fn swipe_decision(axis: SwipeAxis, progress: f64, foreground: ForegroundState) -> crate::gesture_trace::Decision {
    use crate::gesture_trace::Decision::*;
    match axis {
        // Task View keeps its existing Win+Tab toggle on up, Esc on down.
        SwipeAxis::Vertical if progress < 0.0 && foreground.desktop_showing => RestoreDesktop,
        SwipeAxis::Vertical if progress < 0.0 => TaskView,
        SwipeAxis::Vertical if progress > 0.0 && foreground.task_view_showing => TaskViewClose,
        SwipeAxis::Vertical if progress > 0.0 && foreground.desktop_showing => DesktopNoop,
        SwipeAxis::Vertical if progress > 0.0 => ShowDesktop,
        SwipeAxis::Pinch if progress < 0.0 && !foreground.start_open => StartOpen,
        SwipeAxis::Pinch if progress > 0.0 && foreground.start_open => StartClose,
        SwipeAxis::Pinch if progress != 0.0 => StartNoop,
        SwipeAxis::Horizontal if (progress > 0.0) == HORIZONTAL_POSITIVE_GOES_RIGHT => DesktopRight,
        SwipeAxis::Horizontal => DesktopLeft,
        _ => WrongDirection,
    }
}

fn swipe_action(
    axis: SwipeAxis,
    progress: f64,
    peak_abs_progress: f64,
    foreground: ForegroundState,
) -> Option<WinAction> {
    use crate::gesture_trace::Decision::*;
    if peak_abs_progress < SWIPE_PROGRESS_FLOOR {
        return None;
    }
    let keys = match swipe_decision(axis, progress, foreground) {
        TaskView => vec![WIN, TAB],
        TaskViewClose | StartClose => vec![ESC],
        ShowDesktop | RestoreDesktop => vec![WIN, D],
        StartOpen => vec![WIN],
        DesktopRight => vec![CTRL, WIN, RIGHT],
        DesktopLeft => vec![CTRL, WIN, LEFT],
        _ => return None,
    };
    Some(WinAction::Shortcut(keys))
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn desktop_swipe_decision_table_uses_the_committed_foreground_sample() {
        use crate::gesture_trace::Decision::*;
        for (state, down_keys, down, up_keys, up) in [
            (
                Some(ForegroundState {
                    task_view_showing: true,
                    ..Default::default()
                }),
                Some(vec![ESC]),
                TaskViewClose,
                vec![WIN, TAB],
                TaskView,
            ),
            (
                Some(ForegroundState {
                    desktop_showing: true,
                    ..Default::default()
                }),
                None,
                DesktopNoop,
                vec![WIN, D],
                RestoreDesktop,
            ),
            (
                Some(ForegroundState::default()),
                Some(vec![WIN, D]),
                ShowDesktop,
                vec![WIN, TAB],
                TaskView,
            ),
            (None, Some(vec![WIN, D]), ShowDesktop, vec![WIN, TAB], TaskView),
        ] {
            for (progress, keys, decision) in [(0.1, down_keys, down), (-0.1, Some(up_keys), up)] {
                let mut mapper = GestureMapper::default();
                let gesture = swipe(SwipeAxis::Vertical, GesturePhase::Ended, progress);
                let actions = mapper.map_with_foreground_query(&gesture, |axis| {
                    assert_eq!(axis, SwipeAxis::Vertical);
                    state.unwrap_or_default() // Query failure is the not-shown sample.
                });
                assert_eq!(actions, keys.map(WinAction::Shortcut).into_iter().collect::<Vec<_>>());
                let row = mapper.observe(&gesture, &actions, 100, 1, None).unwrap();
                assert_eq!(row.decision, decision);
                assert_eq!(row.task_view_showing, Some(state.unwrap_or_default().task_view_showing));
                assert_eq!(row.desktop_showing, Some(state.unwrap_or_default().desktop_showing));
                assert_eq!(row.start_open, None);
            }
        }
    }

    #[test]
    fn dock_pinch_start_decision_table_is_idempotent_and_failure_means_closed() {
        use crate::gesture_trace::Decision::*;
        for (start_open, close_keys, close, open_keys, open) in [
            (Some(true), None, StartNoop, Some(vec![ESC]), StartClose),
            (Some(false), Some(vec![WIN]), StartOpen, None, StartNoop),
            (None, Some(vec![WIN]), StartOpen, None, StartNoop),
        ] {
            for (progress, keys, decision) in [(-0.1, close_keys, close), (0.1, open_keys, open)] {
                let mut mapper = GestureMapper::default();
                let gesture = swipe(SwipeAxis::Pinch, GesturePhase::Ended, progress);
                let actions = mapper.map_with_foreground_query(&gesture, |axis| {
                    assert_eq!(axis, SwipeAxis::Pinch);
                    ForegroundState {
                        start_open: start_open.unwrap_or(false),
                        ..Default::default()
                    }
                });
                assert_eq!(actions, keys.map(WinAction::Shortcut).into_iter().collect::<Vec<_>>());
                let row = mapper.observe(&gesture, &actions, 100, 1, None).unwrap();
                assert_eq!(row.decision, decision);
                assert_eq!(row.start_open, Some(start_open.unwrap_or(false)));
                assert_eq!(row.task_view_showing, None);
                assert_eq!(row.desktop_showing, None);
            }
        }
    }

    #[test]
    fn all_swipe_axes_preserve_cancel_floor_and_decision_time_query_gates() {
        for axis in [SwipeAxis::Vertical, SwipeAxis::Horizontal, SwipeAxis::Pinch] {
            for progress in [-0.1, 0.1] {
                for phase in [GesturePhase::Began, GesturePhase::Changed, GesturePhase::Cancelled] {
                    let mut mapper = GestureMapper::default();
                    assert!(
                        mapper
                            .map_with_foreground_query(&swipe(axis, phase, progress), |_| panic!("not committed"))
                            .is_empty()
                    );
                }
                let mut mapper = GestureMapper::default();
                assert!(
                    mapper
                        .map_with_foreground_query(
                            &swipe(axis, GesturePhase::Ended, progress.signum() * 0.014),
                            |_| panic!("below floor")
                        )
                        .is_empty()
                );
                for end in [0.05, 0.014] {
                    let mut mapper = GestureMapper::default();
                    mapper.map_with_foreground_query(
                        &swipe(axis, GesturePhase::Changed, progress.signum() * 0.05),
                        |_| panic!("changed"),
                    );
                    let gesture = swipe(axis, GesturePhase::Ended, progress.signum() * end);
                    let actions = mapper.map_with_foreground_query(&gesture, |_| ForegroundState::default());
                    // Pinch-open is a deliberate no-op when Start is closed.
                    assert_eq!(actions.is_empty(), axis == SwipeAxis::Pinch && progress > 0.);
                }
            }
        }
    }

    #[test]
    fn committed_noop_stays_a_noop_when_recording_starts_after_peak_progress() {
        for (axis, state, decision) in [
            (
                SwipeAxis::Vertical,
                ForegroundState {
                    desktop_showing: true,
                    ..Default::default()
                },
                crate::gesture_trace::Decision::DesktopNoop,
            ),
            (
                SwipeAxis::Pinch,
                ForegroundState::default(),
                crate::gesture_trace::Decision::StartNoop,
            ),
        ] {
            let mut mapper = GestureMapper::default();
            mapper.map_with_foreground_query(&swipe(axis, GesturePhase::Changed, 0.1), |_| panic!("not committed"));
            let gesture = swipe(axis, GesturePhase::Ended, 0.014);
            let actions = mapper.map_with_foreground_query(&gesture, |_| state);
            assert!(actions.is_empty());
            let row = mapper.observe(&gesture, &actions, 100, 1, None).unwrap();
            assert!(row.missing_begin);
            assert_eq!(row.decision, decision);
        }
    }

    #[test]
    fn down_shows_desktop_when_foreground_query_reports_nothing() {
        let mut mapper = GestureMapper::default();
        assert_eq!(
            mapper.map(&swipe(SwipeAxis::Vertical, GesturePhase::Ended, 0.1)),
            vec![WinAction::Shortcut(vec![WIN, KeyCode::keyboard(0x07)])]
        );
    }

    #[test]
    fn swipe_direction_and_task_view_state_preserve_other_shortcuts() {
        for showing in [false, true] {
            for (axis, progress, expected) in [
                (SwipeAxis::Vertical, -0.1, Some(vec![WIN, TAB])),
                (
                    SwipeAxis::Vertical,
                    0.1,
                    Some(if showing { vec![ESC] } else { vec![WIN, D] }),
                ),
                (SwipeAxis::Vertical, 0., None),
                (SwipeAxis::Horizontal, -0.1, Some(vec![CTRL, WIN, LEFT])),
                (SwipeAxis::Horizontal, 0.1, Some(vec![CTRL, WIN, RIGHT])),
                (SwipeAxis::Pinch, -0.1, Some(vec![WIN])),
                (SwipeAxis::Pinch, 0.1, None),
            ] {
                assert_eq!(
                    swipe_action(
                        axis,
                        progress,
                        SWIPE_PROGRESS_FLOOR,
                        ForegroundState {
                            task_view_showing: showing,
                            ..Default::default()
                        }
                    ),
                    expected.map(WinAction::Shortcut),
                    "{axis:?}, {progress}, showing={showing}"
                );
                assert_eq!(
                    swipe_action(
                        axis,
                        progress,
                        0.014,
                        ForegroundState {
                            task_view_showing: showing,
                            ..Default::default()
                        }
                    ),
                    None
                );
            }
        }
    }

    #[test]
    fn foreground_is_queried_only_at_a_committed_supported_swipe() {
        use std::cell::Cell;
        let queries = Cell::new(0);
        let mut m = GestureMapper::default();
        for (axis, phase, progress, queried, keys) in [
            (SwipeAxis::Vertical, GesturePhase::Began, 0., false, None),
            (SwipeAxis::Vertical, GesturePhase::Changed, 0.1, false, None),
            (SwipeAxis::Vertical, GesturePhase::Cancelled, 0.1, false, None),
            (SwipeAxis::Vertical, GesturePhase::Ended, 0.014, false, None),
            (SwipeAxis::Pinch, GesturePhase::Ended, 0.1, true, None),
            (
                SwipeAxis::Horizontal,
                GesturePhase::Ended,
                0.1,
                true,
                Some(vec![CTRL, WIN, RIGHT]),
            ),
            (SwipeAxis::Vertical, GesturePhase::Began, 0., false, None),
            (SwipeAxis::Vertical, GesturePhase::Changed, 0.1, false, None),
            // Peak survives retreat; foreground is sampled at Ended, not Changed.
            (SwipeAxis::Vertical, GesturePhase::Ended, 0.014, true, Some(vec![ESC])),
        ] {
            let before = queries.get();
            let g = swipe(axis, phase, progress);
            let actions = m.map_with_foreground_query(&g, |_| {
                queries.set(queries.get() + 1);
                ForegroundState {
                    task_view_showing: true,
                    ..Default::default()
                }
            });
            assert_eq!(queries.get() - before, usize::from(queried));
            assert_eq!(
                m.task_view_showing,
                (queried && axis != SwipeAxis::Pinch).then_some(true)
            );
            assert_eq!(actions, keys.map(WinAction::Shortcut).into_iter().collect::<Vec<_>>());
            let row = m.observe(&g, &actions, 100, 1, None);
            if let Some(row) = row {
                assert_eq!(
                    row.task_view_showing,
                    (queried && axis != SwipeAxis::Pinch).then_some(true)
                );
                if actions == vec![WinAction::Shortcut(vec![ESC])] {
                    assert_eq!(row.decision, crate::gesture_trace::Decision::TaskViewClose);
                }
            }
        }
        assert_eq!(queries.get(), 3);
        // The next gesture must not inherit a stale showing=true sample.
        let g = swipe(SwipeAxis::Vertical, GesturePhase::Ended, 0.1);
        let actions = m.map_with_foreground_query(&g, |_| ForegroundState::default());
        assert_eq!(actions, vec![WinAction::Shortcut(vec![WIN, D])]);
        let row = m.observe(&g, &actions, 200, 1, None).unwrap();
        assert_eq!(row.task_view_showing, Some(false));
        assert_eq!(row.decision, crate::gesture_trace::Decision::ShowDesktop);
        m.map_with_foreground_query(
            &Gesture::Magnify {
                phase: GesturePhase::Changed,
                amount: 0.2,
            },
            |_| panic!("magnify must not query the foreground"),
        );
        assert_eq!(m.task_view_showing, None);
    }

    #[test]
    fn recorded_decisions_use_peak_progress_and_actual_actions() {
        use crate::gesture_trace::Decision;
        for (axis, progress, peak, phase, decision) in [
            (
                SwipeAxis::Vertical,
                -0.014,
                0.014,
                GesturePhase::Ended,
                Decision::BelowThreshold,
            ),
            (
                SwipeAxis::Vertical,
                0.1,
                0.1,
                GesturePhase::Ended,
                Decision::ShowDesktop,
            ),
            (SwipeAxis::Vertical, -0.1, 0.1, GesturePhase::Ended, Decision::TaskView),
            (
                SwipeAxis::Vertical,
                -0.014,
                0.1,
                GesturePhase::Ended,
                Decision::TaskView,
            ),
            (
                SwipeAxis::Horizontal,
                0.1,
                0.1,
                GesturePhase::Ended,
                Decision::DesktopRight,
            ),
            (
                SwipeAxis::Horizontal,
                -0.1,
                0.1,
                GesturePhase::Ended,
                Decision::DesktopLeft,
            ),
            (SwipeAxis::Pinch, 0.1, 0.1, GesturePhase::Ended, Decision::StartNoop),
            (SwipeAxis::Pinch, -0.1, 0.1, GesturePhase::Ended, Decision::StartOpen),
            (
                SwipeAxis::Vertical,
                -0.4,
                0.4,
                GesturePhase::Cancelled,
                Decision::Cancelled,
            ),
        ] {
            let mut m = GestureMapper::default();
            for (g, at) in [
                (swipe(axis, GesturePhase::Began, 0.), 0),
                (swipe(axis, GesturePhase::Changed, peak), 100),
                (swipe(axis, phase, progress), 200),
            ] {
                let actions = m.map_actions(&g);
                let row = m.observe(&g, &actions, at, 1, None);
                if at == 200 {
                    let row = row.unwrap();
                    assert_eq!(row.decision, decision);
                    assert_eq!(row.threshold, Some(SWIPE_PROGRESS_FLOOR));
                    assert_eq!(row.progress, progress);
                    assert_eq!(row.peak_abs_progress, peak);
                    assert_eq!(row.duration_us, 200);
                    assert_eq!(row.changed_events, 1);
                } else {
                    assert_eq!(row.is_some(), at == 0);
                }
            }
        }
    }
    #[test]
    fn magnify_summary_counts_produced_notches_even_when_direction_reverses() {
        let mut m = GestureMapper::default();
        for (phase, amount, at) in [
            (GesturePhase::Began, 0., 0),
            (GesturePhase::Changed, 0.25, 100),
            (GesturePhase::Changed, -0.25, 200),
            (GesturePhase::Ended, 0., 300),
        ] {
            let g = Gesture::Magnify { phase, amount };
            let actions = m.map_actions(&g);
            let row = m.observe(&g, &actions, at, 1, None);
            if phase == GesturePhase::Ended {
                let row = row.unwrap();
                assert_eq!(row.ctrl_wheel_notches, 4);
                assert_eq!(row.ctrl_wheel_balance, 0);
                assert_eq!(row.total_amount, 0.);
                assert_eq!(row.changed_events, 2);
                assert_eq!(row.duration_us, 300);
                assert_eq!(row.threshold, Some(MAGNIFY_PER_NOTCH));
            }
        }
    }
    #[test]
    fn magnify_cancelled_amount_can_still_produce_existing_mapper_action() {
        let mut m = GestureMapper::default();
        let began = Gesture::Magnify {
            phase: GesturePhase::Began,
            amount: 0.,
        };
        let actions = m.map_actions(&began);
        m.observe(&began, &actions, 0, 1, None);
        let cancelled = Gesture::Magnify {
            phase: GesturePhase::Cancelled,
            amount: 0.25,
        };
        let actions = m.map_actions(&cancelled);
        assert_eq!(actions, vec![WinAction::CtrlWheel(2)]);
        let row = m.observe(&cancelled, &actions, 100, 1, None).unwrap();
        assert_eq!(row.ctrl_wheel_notches, 2);
        assert_eq!(row.decision, crate::gesture_trace::Decision::CtrlWheel);
        assert_eq!(row.phase, GesturePhase::Cancelled);
        assert_eq!(row.total_amount, 0.25);
    }

    fn swipe(axis: SwipeAxis, phase: GesturePhase, progress: f64) -> Gesture {
        Gesture::DockSwipe {
            axis,
            phase,
            progress,
            velocity: 0.0,
        }
    }

    fn run(m: &mut GestureMapper, axis: SwipeAxis, to: f64, end: GesturePhase) -> Vec<WinAction> {
        let mut out = m.map(&swipe(axis, GesturePhase::Began, 0.0));
        for i in 1..=5 {
            out.extend(m.map(&swipe(axis, GesturePhase::Changed, to * f64::from(i) / 5.0)));
        }
        out.extend(m.map(&swipe(axis, end, to)));
        out
    }

    #[test]
    fn dock_swipe_fragments_and_cancellations_do_not_trigger_on_any_axis() {
        for (axis, progress) in [
            (SwipeAxis::Vertical, -1.0),
            (SwipeAxis::Horizontal, -1.0),
            (SwipeAxis::Horizontal, 1.0),
            (SwipeAxis::Pinch, 1.0),
        ] {
            let mut m = GestureMapper::default();
            for (amount, phase) in [
                (0.4, GesturePhase::Cancelled),
                (0.014, GesturePhase::Ended),
                (0.049, GesturePhase::Ended),
            ] {
                assert!(run(&mut m, axis, progress * amount, phase).is_empty(), "{axis:?}");
            }
        }
    }

    #[test]
    fn dock_swipes_ended_at_point_one_and_at_the_floor_trigger_on_each_axis() {
        for (axis, progress, keys) in [
            (SwipeAxis::Vertical, -1.0, vec![WIN, TAB]),
            (SwipeAxis::Horizontal, -1.0, vec![CTRL, WIN, LEFT]),
            (SwipeAxis::Horizontal, 1.0, vec![CTRL, WIN, RIGHT]),
        ] {
            for amount in [0.1, SWIPE_PROGRESS_FLOOR] {
                assert_eq!(
                    run(
                        &mut GestureMapper::default(),
                        axis,
                        progress * amount,
                        GesturePhase::Ended
                    ),
                    vec![WinAction::Shortcut(keys.clone())],
                    "{axis:?} at {amount}"
                );
            }
        }
    }

    #[test]
    fn dock_swipe_peak_survives_retreat_and_final_sign_sets_direction() {
        let mut m = GestureMapper::default();
        let axis = SwipeAxis::Horizontal;
        assert!(m.map(&swipe(axis, GesturePhase::Began, 0.)).is_empty());
        assert!(m.map(&swipe(axis, GesturePhase::Changed, -0.1)).is_empty());
        assert!(m.map(&swipe(axis, GesturePhase::Changed, 0.014)).is_empty());
        assert_eq!(
            m.map(&swipe(axis, GesturePhase::Ended, 0.014)),
            vec![WinAction::Shortcut(vec![CTRL, WIN, RIGHT])]
        );
        // The completed gesture's peak cannot make a following fragment fire.
        assert!(m.map(&swipe(axis, GesturePhase::Ended, 0.014)).is_empty());
    }

    #[test]
    fn dock_swipe_peak_resets_for_new_gestures_and_axes() {
        let mut m = GestureMapper::default();
        assert!(
            m.map(&swipe(SwipeAxis::Horizontal, GesturePhase::Began, 0.1))
                .is_empty()
        );
        assert!(
            m.map(&swipe(SwipeAxis::Horizontal, GesturePhase::Began, 0.014))
                .is_empty()
        );
        assert!(
            m.map(&swipe(SwipeAxis::Horizontal, GesturePhase::Ended, 0.014))
                .is_empty()
        );
        assert!(
            m.map(&swipe(SwipeAxis::Horizontal, GesturePhase::Changed, 0.1))
                .is_empty()
        );
        assert!(
            m.map(&swipe(SwipeAxis::Vertical, GesturePhase::Ended, -0.014))
                .is_empty()
        );
        // An Ended frame can still commit after takeover without a Began frame.
        assert_eq!(
            m.map(&swipe(SwipeAxis::Vertical, GesturePhase::Ended, -0.1)),
            vec![WinAction::Shortcut(vec![WIN, TAB])]
        );
    }

    #[test]
    fn swipe_up_opens_task_view_once_at_the_end() {
        let mut m = GestureMapper::default();
        assert_eq!(
            run(&mut m, SwipeAxis::Vertical, -0.8, GesturePhase::Ended),
            vec![WinAction::Shortcut(vec![WIN, TAB])]
        );
        assert_eq!(
            run(&mut m, SwipeAxis::Vertical, 0.8, GesturePhase::Ended),
            vec![WinAction::Shortcut(vec![WIN, D])]
        );
        // 不过阈值、取消：什么都不做。
        assert!(run(&mut m, SwipeAxis::Vertical, -0.014, GesturePhase::Ended).is_empty());
        assert!(run(&mut m, SwipeAxis::Vertical, -0.9, GesturePhase::Cancelled).is_empty());
    }

    #[test]
    fn horizontal_swipe_switches_desktop() {
        let mut m = GestureMapper::default();
        assert_eq!(
            run(&mut m, SwipeAxis::Horizontal, 0.6, GesturePhase::Ended),
            vec![WinAction::Shortcut(vec![CTRL, WIN, RIGHT])]
        );
        assert_eq!(
            run(&mut m, SwipeAxis::Horizontal, -0.6, GesturePhase::Ended),
            vec![WinAction::Shortcut(vec![CTRL, WIN, LEFT])]
        );
    }

    #[test]
    fn pinch_opens_start_while_spread_is_a_noop_when_start_is_closed() {
        let mut m = GestureMapper::default();
        assert!(run(&mut m, SwipeAxis::Pinch, 0.7, GesturePhase::Ended).is_empty());
        assert_eq!(
            run(&mut m, SwipeAxis::Pinch, -0.7, GesturePhase::Ended),
            vec![WinAction::Shortcut(vec![WIN])]
        );
    }

    #[test]
    fn magnify_becomes_ctrl_wheel_notches() {
        let mut m = GestureMapper::default();
        let mag = |phase, amount| Gesture::Magnify { phase, amount };
        let mut out = m.map(&mag(GesturePhase::Began, 0.0));
        for _ in 0..7 {
            out.extend(m.map(&mag(GesturePhase::Changed, 0.04)));
        }
        // 0.28 → 两格，余 0.08 在结束时丢掉。
        out.extend(m.map(&mag(GesturePhase::Ended, 0.0)));
        let total: i32 = out
            .iter()
            .map(|a| match a {
                WinAction::CtrlWheel(n) => *n,
                _ => 0,
            })
            .sum();
        assert_eq!(total, 2);
        // 缩小。
        m.map(&mag(GesturePhase::Began, 0.0));
        assert_eq!(
            m.map(&mag(GesturePhase::Changed, -0.25)),
            vec![WinAction::CtrlWheel(-2)]
        );
    }

    #[test]
    fn rotate_and_smart_magnify_are_dropped() {
        let mut m = GestureMapper::default();
        assert!(m.map(&Gesture::SmartMagnify).is_empty());
        assert!(
            m.map(&Gesture::Rotate {
                phase: GesturePhase::Changed,
                degrees: 30.0
            })
            .is_empty()
        );
    }

    #[test]
    fn system_keys() {
        assert_eq!(key_shortcut(KeyCode::new(0x0C, 0x029F)), Some(vec![WIN, TAB]));
        assert_eq!(key_shortcut(KeyCode::new(0x0C, 0x00CF)), Some(vec![WIN, H]));
        assert_eq!(key_shortcut(KeyCode::new(0x0C, 0xE9)), None);
        assert!(key_dropped(KeyCode::new(0x01, 0x9B)));
        assert!(!key_dropped(KeyCode::new(0x0C, 0xE9)));
    }
}
