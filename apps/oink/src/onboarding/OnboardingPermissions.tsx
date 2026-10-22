import { useEffect, useRef, useState } from 'react';
import type { CSSProperties } from 'react';
import { Button, Dialog, Group, Icon, Switch, speed } from 'oink-ui';
import type { DialogProps } from 'oink-ui';
import { api } from '../api/client';
import { agent } from '../api/store';
import type { LaunchAtLoginStatus } from '../api/types';
import { tr } from '../i18n';
import type { MessageKey } from '../i18n';
import { PermissionRow } from '../permissions/PermissionRow';
import { PermissionControl } from '../permissions/PermissionControl';
import type { PermissionId, PrefsApi } from '../settings/prefs';
import type { PermState } from '../settings/SettingsPage';
import { useGrant } from '../settings/useGrant';

const PERMS: Array<{ id: PermissionId; label: MessageKey; desc: MessageKey }> = [
  { id: 'accessibility', label: '辅助功能', desc: '用于向本机注入键鼠事件' },
  { id: 'input', label: '输入监控', desc: '用于控制其它设备' },
  { id: 'network', label: '本地网络', desc: '用于发现和连接局域网设备' },
  { id: 'files', label: '文件提供服务', desc: '用于向本机粘贴和拖拽文件' },
  { id: 'audio', label: '系统音频录制', desc: '用于转发音频到其它设备' },
];

// macOS only: Windows onboarding has no permissions page, launch at login is in Settings.
function initialLaunch(status: LaunchAtLoginStatus | null): boolean | null {
  if (!status) return null;
  if (!status.configured && status.state === 'disabled' && !status.problem) return true;
  return status.state === 'enabled' || status.state === 'requires_approval';
}

export function OnboardingPermissions({ preferences, perms, onGrant, onOpenSettings, container, leaving, onDone }: {
  preferences: PrefsApi;
  perms: Record<PermissionId, PermState>;
  onGrant: (id: PermissionId) => void | Promise<void>;
  onOpenSettings?: (id: PermissionId) => void | Promise<void>;
  container: HTMLElement | null;
  leaving: boolean;
  onDone: () => void;
}) {
  const grant = useGrant(onGrant, onOpenSettings);
  const done = PERMS.every(p => ['granted', 'not_required', 'unsupported'].includes(perms[p.id]));
  const [ask, setAsk] = useState(false);
  const [launch, setLaunch] = useState(() => initialLaunch(preferences.launchStatus));
  const touched = useRef(false);
  const [saving, setSaving] = useState(false);
  const busy = useRef(false);
  useEffect(() => {
    if (!touched.current) setLaunch(initialLaunch(preferences.launchStatus));
  }, [preferences.launchStatus]);

  // The forward action confirms the displayed value even without a switch edit.
  // Keep the latest draft in the automatic grant-completion callback as well.
  const proceed = async () => {
    if (busy.current || leaving) return;
    busy.current = true;
    setSaving(true);
    try {
      const status = preferences.launchStatus ?? await api.launchAtLoginStatus();
      if (status.state === 'unavailable' && status.problem === 'system_unavailable') throw new Error('unavailable');
      if (status.state !== 'unavailable') {
        await preferences.confirmLaunchAtLogin(launch ?? initialLaunch(status)!);
      }
      onDone();
    } catch {
      agent.fail('unavailable');
      busy.current = false;
      setSaving(false);
    }
  };
  const proceedRef = useRef(proceed);
  proceedRef.current = proceed;
  useEffect(() => {
    if (!done) return;
    const timer = window.setTimeout(() => { void proceedRef.current(); }, 900 * Math.max(speed(), 0.2));
    return () => window.clearTimeout(timer);
  }, [done]);
  const containedProps = { contained: true } as Partial<DialogProps>;

  return (
    <section className="ob-page ob-perms" data-leaving={leaving ? '' : undefined}>
      <div className="ob-main ok-scroll">
        <div className="ob-col">
          <h1 className="ob-title ob-in">{tr('授予必要的系统权限')}</h1>
          <p className="ob-sub ob-in" style={{ '--d': '.05s' } as CSSProperties}>
            {tr('Oink 需要依靠系统权限来实现共享功能，缺少权限会影响部分功能可用性。')}
          </p>
          <Group className="ob-perms__card ob-in" style={{ '--d': '.1s' } as CSSProperties}>
            <PermissionRow className="ob-perms__row" label={tr('开机自动启动')} description={tr('开机后静默启动 Oink')}>
              <Switch aria-label={tr('开机自动启动')} checked={launch ?? false} disabled={launch === null || saving || preferences.launchStatus?.state === 'unavailable'} onChange={value => { touched.current = true; setLaunch(value); }} />
            </PermissionRow>
            {PERMS.filter(p => perms[p.id] !== 'not_required').map(p => (
              <PermissionRow key={p.id} className="ob-perms__row" label={tr(p.label)} description={p.id === 'input' && perms.accessibility === 'granted' && perms.input === 'granted' ? tr('辅助功能已包含输入监控，无需重复授权') : tr(p.desc)}>
                <PermissionControl id={p.id} state={perms[p.id]} grant={grant}/>
              </PermissionRow>
            ))}
          </Group>
        </div>
      </div>
      <footer className="ob-foot ob-in" style={{ '--d': '.15s' } as CSSProperties}>
        {!done
          ? <Button variant="ghost" size="sm" className="ob-link" disabled={saving} onClick={() => setAsk(true)}>{tr('跳过')}</Button>
          : <p className="ob-done" role="status"><Icon name="check-circle" />{tr('已全部授权')}</p>}
      </footer>
      <Dialog
        {...containedProps} className="ob-skip-dialog" open={ask} onOpenChange={setAsk} container={container} size={360}
        title={tr('跳过剩余授权')}
        description={tr('未授予权限的功能暂时无法使用，您可随时在 设置 › 权限 中进行授权。')}
        actions={<>
          <Button onClick={() => setAsk(false)}>{tr('取消')}</Button>
          <Button variant="accent" onClick={() => { setAsk(false); void proceed(); }}>{tr('确认')}</Button>
        </>}
      />
    </section>
  );
}
