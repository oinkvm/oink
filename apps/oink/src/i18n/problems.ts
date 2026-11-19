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

export const startupMessages = {
  waiting: ['正在等待 Oink 服务启动…', 'Waiting for the Oink service to start…', '电脑刚开机或刚装好 Oink 时，服务需要一点时间准备。准备好后会自动连接，不用做任何操作', "Right after startup or installation, the service needs a moment to get ready. Oink connects automatically when it’s ready"],
  missing: ['Oink 服务没有安装', "The Oink service isn't installed", 'Oink 需要的系统服务不在这台电脑上，可能是安装没有完成。请下载最新安装包，重新安装 Oink', "A system service Oink needs isn't on this PC. Download the latest installer and reinstall Oink"],
  stopped: ['Oink 服务已停止', 'The Oink service is stopped', '请打开「服务」，找到 OinkInput 并启动它。服务就绪后，Oink 会自动连接。如果还是不行，请重新安装 Oink', 'Open Services, find OinkInput and start it. Oink will connect automatically when the service is ready. If that doesn’t help, reinstall Oink'],
  disabled: ['Oink 服务已被禁用', 'The Oink service is disabled', '请打开「服务」，把 OinkInput 的启动类型改为「自动」并启动它。服务就绪后，Oink 会自动连接', 'In Services, set OinkInput to Automatic and start it. Oink will connect automatically when the service is ready'],
  session: ['请在这台电脑前登录后使用', 'Sign in at this PC to use Oink', '这个登录会话已断开，或电脑已切换到其他用户，Oink 暂时不能在这里运行。回到这台电脑前登录后会自动重试', 'This session is disconnected or another user is active. Oink retries when this account is active again'],
  update: ['上次更新没有完成', "The last update didn't finish", '运行最新安装包可以修复这个问题；如果仍然失败，请复制诊断信息发给我们', 'Run the latest installer to repair the installation. If it still fails, copy the diagnostic info and send it to us'],
  rdp: ['远程桌面期间共享已暂停', 'Sharing is paused during Remote Desktop', '回到本机登录后自动恢复', 'It resumes automatically when you sign in at this PC again'],
  exited: ['Oink 后台启动后意外退出', "Oink's background process quit unexpectedly", '这通常是偶发问题，请点「重试」。如果反复出现，请复制诊断信息发给我们', 'Click Retry. If this keeps happening, copy the diagnostic info and send it to us'],
  identity: ['Oink 服务身份验证失败', 'Oink service identity verification failed', '请复制诊断信息发给我们', 'Copy the diagnostic info and send it to us'],
  unknown: ['无法连接 Oink 后台', 'Unable to connect to the Oink background process', '请复制诊断信息发给我们', 'Copy the diagnostic info and send it to us'],
} as const;
export function startupKind(raw: string): keyof typeof startupMessages {
  if (raw.startsWith('waiting:')) return 'waiting';
  if (raw === 'rdp') return raw;
  for (const code of ['missing', 'disabled', 'stopped', 'session'] as const) {
    if (raw.startsWith('startup_' + code + ':')) return code;
  }
  if (raw === 'oink_update_in_progress') return 'update';
  if (raw.startsWith('backend_exited:')) return 'exited';
  if (raw.includes('identity verification failed')) return 'identity';
  return 'unknown';
}
