//! 显示器描述（由「屏幕与布局」的平台实现填写，布局、路由、标定、界面使用）。

use crate::geom::{Rect, Size};
use serde::{Deserialize, Serialize};

/// Fixed reference diagonal used by every layout, independent of connected displays.
pub const REFERENCE_DIAGONAL_METERS: f64 = 0.686;

/// 本机的一块显示器。
///
/// **本机全局坐标**（`bounds` 所用坐标系）就是本机系统的光标坐标：
/// - macOS：point，主屏左上角为原点，y 向下（Quartz 全局坐标）；
/// - Windows：物理像素，虚拟桌面坐标（进程为 Per-Monitor-V2 DPI 感知）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    /// 稳定标识：重启、重新插拔后不变（macOS：显示器 UUID；Windows：显示器设备实例路径）。
    pub key: String,
    /// 给人看的名字，如「Built-in Retina Display」「DELL U2720Q」。
    pub name: String,
    /// 在本机全局坐标中的位置与大小。
    pub bounds: Rect,
    /// 实际像素数（旋转后的方向）。
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// Optional native measurement normalized by the fixed reference diagonal.
    pub reference_extent: Option<Size>,
    /// 顺时针旋转角度：0 / 90 / 180 / 270。
    pub rotation_deg: u16,
    pub is_primary: bool,
    pub is_builtin: bool,
    /// 虚拟显示器（远程桌面软件的虚拟屏、无头显示器等），界面上默认隐藏。
    pub is_virtual: bool,
}

impl DisplayInfo {
    /// Reference length per native coordinate unit, when the extent is known.
    pub fn reference_per_native_unit(&self) -> Option<f64> {
        self.reference_extent
            .filter(|s| s.w > 0.0 && self.bounds.w > 0.0)
            .map(|s| s.w / self.bounds.w)
    }

    /// 每个本机全局坐标单位包含的实际像素数（macOS Retina 通常为 2，Windows 恒为 1）。
    pub fn pixels_per_unit(&self) -> f64 {
        if self.bounds.w > 0.0 {
            f64::from(self.pixel_width) / self.bounds.w
        } else {
            1.0
        }
    }
}
