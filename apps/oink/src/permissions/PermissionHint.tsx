import type { ReactElement } from 'react';
import { Tooltip } from 'oink-ui';
import { tr, trNodes } from '../i18n';
import type { MessageKey } from '../i18n/catalog';
import './permissionHint.css';

export type PermissionHintKind = 'accessibility' | 'input_monitoring' | 'microphone' | 'virtual_microphone' | 'permissions';
const copy: Record<PermissionHintKind, { local: MessageKey; remote: MessageKey }> = {
  permissions: { local: '权限设置：{0}', remote: '权限设置' },
  accessibility: { local: '缺少辅助功能权限，{0}', remote: '缺少辅助功能权限' },
  input_monitoring: { local: '缺少输入监控权限，{0}', remote: '缺少输入监控权限' },
  microphone: { local: '缺少麦克风权限，{0}', remote: '缺少麦克风权限' },
  virtual_microphone: { local: '不可用，请安装虚拟麦克风，{0}', remote: '缺少虚拟麦克风' },
};

/** Own both the trigger and tooltip action so local/remote behavior stays identical
 * across device indicators and disabled menu controls. */
export function PermissionHint({ kind, local, onGrant, disabled, children, lines }: {
  kind: PermissionHintKind;
  local: boolean;
  onGrant?: () => void;
  disabled?: boolean;
  lines?: string[];
  children: (hint: { label: string; onGrant?: () => void }) => ReactElement;
}) {
  const grant = local ? onGrant : undefined;
  const action = kind === 'virtual_microphone' ? tr('前往音频设置') : kind === 'permissions' ? tr('打开设置') : tr('hint|前往授权');
  const label = local ? tr(copy[kind].local, action) : tr(copy[kind].remote);
  const content = grant ? <span>{trNodes(copy[kind].local,
    <button key="grant" type="button" className="permission-hint__action ok-focusable" onClick={grant}>{action}</button>)}</span> : label;
  return <Tooltip interactive={!!grant} content={lines ? <span className="dev-ico__tip">{lines.map((line,index)=><span key={index}>{line}{index===1 && grant && <> <button type="button" className="permission-hint__action ok-focusable" onClick={grant}>{action}</button></>}</span>)}</span> : content} disabled={disabled}>
    {children({ label, onGrant: grant })}
  </Tooltip>;
}
