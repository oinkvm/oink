import { tr } from './index';
import type { MessageKey } from './catalog';

/** User-facing text for the backend's `UserFacingProblem` codes and the UI's own failure codes. */
const PROBLEMS: Record<string, MessageKey> = {
  local_service_unavailable: '连不上 Oink 后台',
  link_lost: '连接已断开',
  timeout: '操作超时',
  no_route: '没有通往目标网络的路由',
  no_network_interface: '没有可用的网络接口',
  peer_unreachable: '目标设备不可达',
  socket_error: '网络套接字异常',
  network_stopped: '网络服务已停止',
  invalid_address: '请输入有效的 IPv4 地址和端口',
  storage_full: '存储空间已满',
  invalid_value: '设置值无效',
  state_conflict: '状态已变化，请重试',
  stale_boot: '设备已重新启动，请重试',
  not_member: '设备不在当前设备组中',
  resource_fault: '共享功能不可用',
  apply_failed: '无法应用设置',
  not_exit: '音频出口已变化',
  route_changed: '音频出口已变化',
  output_changed: '音频输出设备已变化',
  output_unavailable: '音频输出设备不可用',
  superseded: '已由更新的操作接替',
  unsupported_option: '不支持此设置',
  permission_required: '系统权限未授予',
  permission_denied: '系统权限被拒绝',
  capture_unavailable: '共享功能不可用',
  injection_unavailable: '无法注入输入',
  secure_input: '安全输入已启用',
  device_unavailable: '设备不可用',
  cleanup_failed: '清理未完成',
  unsupported: '不支持此操作',
  backend_failed: '后台服务异常',
  microphone_loopback: '请在来源设备选择硬件麦克风，不能共享虚拟回环输入',
  unavailable: '连不上 Oink 后台',
  outcome_unknown: '无法确认结果',
  stale_revision: '设备状态已变化，请重试',
  offline: '设备已离线',
  busy: '操作进行中',
  emergency_stop: '已收到暂停设备组共享指令',
  persistence_failed: '无法保存设置',
};

export function problemText(code: string): string {
  return tr(PROBLEMS[code] ?? '不可用');
}
