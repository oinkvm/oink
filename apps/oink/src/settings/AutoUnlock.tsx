import { useEffect, useState } from 'react';
import { Button, Dialog, Field, Icon, Input, Tooltip } from 'oink-ui';
import type { DialogProps } from 'oink-ui';
import { tr } from '../i18n';
import type { Platform } from '../window/DesktopWindow';
import type { AutoUnlockFailure } from '../api/client';

const contained = { contained: true } as Partial<DialogProps>;

/** Settings › General › 自动解锁本机: an Enable button until a login password is
 * saved, then a green check with Change password / Turn off. The password only
 * travels to the local backend, which encrypts and keeps it on this device. */
export function AutoUnlockControl({ platform, enabled, failure, windowsHelloOnly, label, onSave, onDisable }: {
  platform: Platform;
  enabled: boolean;
  failure: AutoUnlockFailure | null;
  windowsHelloOnly: boolean;
  label: string;
  /** Resolves to a refusal text, or null when the password was saved. */
  onSave: (password: string) => Promise<string | null>;
  onDisable: () => Promise<void>;
}) {
  const [editing, setEditing] = useState<'enable' | 'change' | null>(null);
  const [pending, setPending] = useState(false);
  const helloOnly = platform === 'win' && windowsHelloOnly;
  if (!enabled) return <>
    {helloOnly
      ? <Tooltip interactive={false} content={tr('请先在系统设置中关闭“仅允许 Windows Hello 登录”')}>
        <span className="setp__disabled-btn"><Button size="sm" disabled>{tr('启用')}</Button></span>
      </Tooltip>
      : <Button size="sm" onClick={() => setEditing('enable')}>{tr('启用')}</Button>}
    <PasswordDialog platform={platform} mode={editing} onClose={() => setEditing(null)} onSave={onSave}/>
  </>;
  const failed = failure !== null || helloOnly;
  return <span className="setp__btns">
    <Icon name={failed ? 'alert' : 'check-circle'} className={failed ? 'setp__unlock-failed' : 'setp__unlock-on'} aria-label={tr('{0}已开启', label)}/>
    <Button size="sm" disabled={pending} onClick={() => setEditing('change')}>{tr('修改密码')}</Button>
    <Button size="sm" loading={pending} onClick={async () => { setPending(true); try { await onDisable(); } finally { setPending(false); } }}>{tr('关闭')}</Button>
    <PasswordDialog platform={platform} mode={editing} onClose={() => setEditing(null)} onSave={onSave}/>
  </span>;
}

function PasswordDialog({ platform, mode, onClose, onSave }: {
  platform: Platform;
  mode: 'enable' | 'change' | null;
  onClose: () => void;
  onSave: (password: string) => Promise<string | null>;
}) {
  const [password, setPassword] = useState('');
  const [refusal, setRefusal] = useState<string | null>(null);
  const [pending, setPending] = useState(false);
  // Never keep a typed password around once the dialog closes.
  useEffect(() => { setPassword(''); setRefusal(null); }, [mode]);
  const submit = async () => {
    if (!password || pending) return;
    setPending(true);
    try {
      const refused = await onSave(password);
      if (refused) setRefusal(refused); else onClose();
    } finally { setPending(false); }
  };
  return <Dialog {...contained} open={mode !== null} onOpenChange={o => { if (!o) onClose(); }} size={400} as="form" onSubmit={() => { void submit(); }}
    title={mode === 'change' ? tr('修改登录密码') : tr('自动解锁本机')}
    description={tr('输入本机的登录密码。密码加密存储在本机，用于本机锁定时自动解锁。')}
    actions={<><Button onClick={onClose}>{tr('取消')}</Button><Button type="submit" variant="accent" loading={pending} disabled={!password}>{tr('保存')}</Button></>}>
    <Field label={platform === 'win' ? tr('登录密码（非 PIN）') : tr('登录密码')} error={refusal}>
      <Input type="password" value={password} autoFocus autoComplete="off" onChange={v => { setPassword(v); setRefusal(null); }}/>
    </Field>
  </Dialog>;
}
