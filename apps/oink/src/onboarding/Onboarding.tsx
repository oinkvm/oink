import { useEffect, useId, useRef, useState } from 'react';
import type { CSSProperties } from 'react';
import { Button, ColorSwatches, Divider, Dots, DropdownMenu, Group, Icon, MenuRadioGroup, MenuRadioItem, MenuSeparator, Row, Switch, speed, useOink } from 'oink-ui';
import { ACCENT_SWATCHES } from '../settings/appearance';
import { FoldItem, NearbyRow, RefreshButton } from '../pairing/AddPanel';
import { ManualSearch, PairBody } from '../pairing/PairDialog';
import { Morph } from '../pairing/Morph';
import { agent, useAgent } from '../api/store';
import { shell } from '../api/client';
import { pairingApi } from '../api/pairing';
import { LANGUAGES, secondaryName, tr, trNodes, useLanguage } from '../i18n';
import type { PrefsApi } from '../settings/prefs';
import type { Pairing } from '../pairing/usePairing';
import { useListPresence } from '../pairing/useListPresence';
import type { PermissionId } from '../settings/prefs';
import type { PermState } from '../settings/SettingsPage';
import type { Platform } from '../window/DesktopWindow';
import { OnboardingPermissions } from './OnboardingPermissions';
import { ThemeCards } from './ThemeCards';
import { FLY_MS } from './flight';
import { WelcomeLogo } from './WelcomeLogo';
import './onboarding.css';

export type OnboardingStep = 'welcome' | 'perms' | 'telemetry' | 'connect';

const EXIT_MS: Record<OnboardingStep, number> = { welcome: FLY_MS, perms: 260, telemetry: 260, connect: 260 };

const PRIVACY_URL = 'https://github.com/oinkvm/oink/blob/main/PRIVACY.md';

export interface OnboardingProps {
  platform: Platform;
  pairing: Pairing;
  perms: Record<PermissionId, PermState>;
  onGrant: (id: PermissionId) => void | Promise<void>;
  onOpenSettings?: (id: PermissionId) => void | Promise<void>;

  container: HTMLElement | null;

  onSkip: () => void | Promise<boolean | undefined>;

  connected: boolean;

  onFinish: () => void | Promise<boolean | undefined>;

  leaving: boolean;

  version: string;

  preferences: PrefsApi;
}

export function Onboarding({ platform, pairing, perms, onGrant, onOpenSettings, container, onSkip, leaving, preferences, version, connected, onFinish }: OnboardingProps) {
  useLanguage();
  const { boot } = useAgent();
  const [step, setStep] = useState<OnboardingStep>('welcome');
  const [exiting, setExiting] = useState(false);
  const [connectReady, setConnectReady] = useState(false);
  useEffect(() => {
    let current = true;
    setConnectReady(false);
    void pairingApi.onboardingConnect(step === 'connect' && !leaving).then(async () => {
      if (step === 'connect' && current) {
        await agent.query('pairings', pairingApi.pairings);
        if (current) setConnectReady(true);
      }
    }).catch(() => agent.fail('unavailable'));
    return () => { current = false; };
  }, [step, leaving, boot]);
  useEffect(() => () => { void pairingApi.onboardingConnect(false).catch(() => agent.fail('unavailable')); }, []);
  const timer = useRef(0);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  const connectedRef = useRef(connected);
  connectedRef.current = connected;

  // Windows has no permissions to grant; launch at login lives in Settings only.
  const nextOf = (s: OnboardingStep): OnboardingStep =>
    s === 'welcome' ? (platform === 'mac' ? 'perms' : 'telemetry') : s === 'telemetry' ? 'connect' : 'telemetry';
  const next = () => {
    if (exiting) return;
    const to = nextOf(step);
    setExiting(true);
    if (to === 'connect' && connectedRef.current) {
      timer.current = window.setTimeout(() => {
        void Promise.resolve(onFinish()).then(saved => { if (saved === false) setExiting(false); });
      }, EXIT_MS[step] * speed());
      return;
    }
    timer.current = window.setTimeout(() => {
      setStep(to);
      setExiting(false);

    }, EXIT_MS[step] * speed());
  };

  return (
    <div className="ob" data-platform={platform} data-step={step} data-leaving={leaving ? '' : undefined} role="region" aria-label={tr('欢迎使用 Oink')}>
      {step === 'welcome' && <Welcome leaving={exiting} version={version} preferences={preferences} onNext={next} />}
      {step === 'perms' && <OnboardingPermissions preferences={preferences} perms={perms} onGrant={onGrant} onOpenSettings={onOpenSettings} container={container} leaving={exiting} onDone={next} />}
      {step === 'telemetry' && <Telemetry leaving={exiting} preferences={preferences} onNext={next} />}
      {step === 'connect' && <Connect pairing={pairing} ready={connectReady} leaving={exiting} onSkip={onSkip} onFinish={onFinish} />}
    </div>
  );
}

export function OnboardingLanguage({ value, onChange, leaving }: { value: string; onChange: (v: string) => void; leaving: boolean }) {
  const current = value === 'auto' ? tr('跟随系统') : LANGUAGES.find((l) => l.id === value)?.autonym ?? value;
  return (
    <span className="ob-lang" data-leaving={leaving ? '' : undefined}>
      <DropdownMenu
        placement="bottom-end" matchTriggerWidth={false}
        trigger={
          <Button variant="ghost" size="sm" iconOnly aria-label={tr('语言：{0}', current)}>
            <Icon name="globe" />
          </Button>
        }
      >
        <MenuRadioGroup aria-label={tr('语言')} value={value} onChange={onChange}>
          <MenuRadioItem value="auto" closeOnSelect>{tr('跟随系统')}</MenuRadioItem>
          <MenuSeparator />
          {LANGUAGES.map((l) => <MenuRadioItem key={l.id} value={l.id} shortcut={secondaryName(l)} textValue={l.autonym} closeOnSelect><span lang={l.id}>{l.autonym}</span></MenuRadioItem>)}
        </MenuRadioGroup>
      </DropdownMenu>
    </span>
  );
}

function Welcome({ leaving, version, preferences, onNext }: { leaving: boolean; version: string; preferences: PrefsApi; onNext: () => void }) {
  const { theme, setTheme, accent, setAccent } = useOink();
  const uid = useId();
  return (
    <section className="ob-page ob-welcome" data-leaving={leaving ? '' : undefined}>
      <div className="ob-welcome__hero">
        <WelcomeLogo leaving={leaving} />
        <p className="ob-ver ob-in" style={{ '--d': '1.75s' } as CSSProperties}>v{version}</p>
      </div>
      <div className="ob-welcome__prefs ob-in" style={{ '--d': '1.95s' } as CSSProperties}>
        <div className="ob-field">
          <span className="ob-field__label" id={`${uid}-accent`}>{tr('主题色')}</span>
          <ColorSwatches aria-labelledby={`${uid}-accent`} colors={ACCENT_SWATCHES} value={accent} onChange={(v) => {
            const swatch = ACCENT_SWATCHES.find(c => c.value === v)!;
            setAccent(v as typeof accent);
            void preferences.saveLocal({ theme_accent: { light: swatch.color, dark: swatch.dark } }).catch(() => { setAccent(accent); agent.fail('unavailable'); });
          }} disabled={!preferences.ready} />
        </div>
        <div className="ob-field">
          <span className="ob-field__label" id={`${uid}-theme`}>{tr('外观')}</span>
          <ThemeCards value={theme} onChange={v => {
            setTheme(v);
            void preferences.saveLocal({ theme_mode: v }).catch(() => { setTheme(theme); agent.fail('unavailable'); });
          }} disabled={!preferences.ready} labelledBy={`${uid}-theme`} />
        </div>
      </div>
      {/* Animate inside the footer's padding. Translating the grid item itself
          extends the welcome scroller by 10px until its entrance ends. */}
      <footer className="ob-foot">
        <div className="ob-in" style={{ '--d': '2.1s' } as CSSProperties}><Button variant="accent" className="ob-next" trailing={<Icon name="arrow-right" />} onClick={onNext}>{tr('下一步')}</Button></div>
      </footer>
    </section>
  );
}

function Telemetry({ leaving, preferences, onNext }: {
  leaving: boolean;
  preferences: PrefsApi;
  onNext: () => void;
}) {
  const [on, setOn] = useState(false);
  const [saving, setSaving] = useState(false);
  const proceed = async () => {
    setSaving(true);
    try { await preferences.chooseReport(on); onNext(); }
    catch { agent.fail('unavailable'); }
    finally { setSaving(false); }
  };
  return (
    <section className="ob-page ob-telemetry" data-leaving={leaving ? '' : undefined}>
      <div className="ob-main">
        <div className="ob-col">
          <h1 className="ob-title ob-in">{tr('帮助我们改进 Oink')}</h1>
          <Group className="ob-perms__card ob-telemetry__card ob-in" style={{ '--d': '.06s' } as CSSProperties}>
            <Row
              label={tr('上报崩溃记录和性能数据')}
              description={
                <>
                  {trNodes('崩溃和性能报告发送至 Sentry（欧盟数据中心），不含您的按键和个人信息，{0}',
                    <a className="ob-a ok-focusable" href={PRIVACY_URL} target="_blank" rel="noreferrer" onClick={e => { e.preventDefault(); void shell.openUrl(PRIVACY_URL).catch(() => agent.fail('unavailable')); }}>{tr('查看隐私政策')}</a>)}
                </>
              }
            >
              <Switch size="lg" aria-label={tr('上报崩溃记录和性能数据')} checked={on} onChange={setOn} />
            </Row>
          </Group>
        </div>
      </div>
      <footer className="ob-foot ob-in" style={{ '--d': '.12s' } as CSSProperties}>
        <Button variant="accent" className="ob-next" trailing={<Icon name="arrow-right" />} loading={saving} disabled={!preferences.report} onClick={() => { void proceed(); }}>{tr('下一步')}</Button>
      </footer>
    </section>
  );
}

function Connect({ pairing, ready, leaving, onSkip, onFinish }: Pick<OnboardingProps, 'pairing' | 'leaving' | 'onSkip' | 'onFinish'> & { ready: boolean }) {
  const { network, overview } = useAgent();
  const SELF = { ips: network?.addresses.map(a => a.ip) ?? [], deviceId: overview?.this_device.fingerprint ?? '—' };
  const { dialog, actions } = pairing;
  const s = dialog.session;
  const [completionFailed, setCompletionFailed] = useState(false);
  const firstKey = useRef(dialog.open ? 0 : s?.key ?? 0);
  const pairingNow = !!s && dialog.mode === 'pair' && (dialog.open || (s.status === 'success' && s.key > firstKey.current));
  useEffect(() => {
    if (!pairingNow || s?.status !== 'success' || leaving) return;
    const timer = window.setTimeout(() => {
      void Promise.resolve(onFinish()).then(saved => setCompletionFailed(saved === false));
    }, 600 * Math.max(speed(), 0.1));
    return () => window.clearTimeout(timer);
  }, [pairingNow, s?.status, leaving]);
  const back = () => {
    if (s?.dir === 'in' && s.status !== 'failed') actions.dismiss();
    else void actions.cancel().catch(() => agent.fail('unavailable'));
  };
  const autoAccepted = useRef(new Set<string>());
  const incoming = pairing.requests.find((r) => !autoAccepted.current.has(r.id));
  useEffect(() => {
    // Wrong-but-retryable codes are still active. Only the terminal failed view
    // yields this screen to a fresh request; a successful view keeps finishing.
    if (!ready || !incoming || (dialog.open && s?.status !== 'failed') || leaving) return;
    autoAccepted.current.add(incoming.id);
    actions.accept(incoming);
  }, [ready, incoming?.id, dialog.open, s?.status, leaving]);

  return (
    <section className="ob-page ob-connect" data-leaving={leaving ? '' : undefined}>
      <div className="ob-main" inert={!ready}>
        <div className="ob-col ob-col--narrow ob-col--fit">
          <Morph immediate={pairingNow && s!.status === 'failed'} viewKey={pairingNow ? `pair-${s!.key}-${s!.status === 'failed' ? 'failed' : s!.status === 'connecting' ? 'connecting' : 'code'}` : 'list'} pending={pairingNow && s!.status === 'connecting' && s!.source !== 'nearby'}>
          {pairingNow ? (
            <div className="ob-pair">
              <div className="ob-pair__head ob-in">
                <Button variant="ghost" size="sm" iconOnly aria-label={tr('返回')} disabled={s!.status === 'success'} onClick={back}>
                  <Icon name="arrow-left" />
                </Button>
                <h1 className="ob-pair__title">{tr('与 {0} 配对', s!.peer.name)}</h1>
              </div>
              <p className="ob-sub ob-pair__lead ob-in" style={{ '--d': '.04s' } as CSSProperties}>
                {s!.dir === 'in'
                  ? s!.source === 'request' ? tr('{0} 请求连接本机，输入对方屏幕上显示的 6 位配对码', s!.peer.name) : tr('输入对方屏幕上显示的 6 位配对码')
                  : tr('在对方的 Oink 里输入下面的配对码')}
              </p>
              <div className="ob-in" style={{ '--d': '.08s' } as CSSProperties}>
                <PairBody session={s!} pairing={pairing} />
              </div>
            </div>
          ) : (
            <>
              <h1 className="ob-title ob-in">{tr('连接你的其他设备')}</h1>
              <p className="ob-sub ob-in" style={{ '--d': '.05s' } as CSSProperties}>
                {tr('在另一台电脑上启动 Oink 完成配对，搜索不到时，检查双方的网络和防火墙配置，或尝试手动输入 IP 连接')}
              </p>
              <div className="ob-devs-wrap ob-in" style={{ '--d': '.1s' } as CSSProperties}>
                <NearbyList pairing={pairing} />
              </div>
              <Divider className="ob-or ob-in" style={{ '--d': '.15s' } as CSSProperties}>{tr('或手动输入 IP')}</Divider>
              <div className="ob-manual ob-in" style={{ '--d': '.2s' } as CSSProperties}>
                <ManualSearch pairing={pairing} autoFocus={false} />
              </div>
            </>
          )}
          </Morph>
        </div>
      </div>
      <footer className="ob-foot ob-foot--connect ob-in" style={{ '--d': '.25s' } as CSSProperties}>
        {completionFailed && <Button variant="accent" onClick={() => { void onFinish(); }}>{tr('继续')}</Button>}
        {!pairingNow && <Button variant="ghost" size="sm" className="ob-link" onClick={onSkip}>{tr('跳过，稍后再连接')}</Button>}
        {}
        {pairingNow && <p className="ob-tip">{tr('TIPS：遇到问题时，长按 ESC 5 秒可暂停共享')}</p>}
        <p className="ob-self">
          <span>{tr('本机 IP')}{' '}<b>{SELF.ips.join(tr('、'))}</b></span>
          <span>{tr('本机 ID')}{' '}<b>{SELF.deviceId}</b></span>
        </p>
      </footer>
    </section>
  );
}

function NearbyList({ pairing }: { pairing: Pairing }) {
  const { nearby, actions } = pairing;
  const near = useListPresence(nearby);
  const uid = useId();
  return (
    <section className="ob-devs" aria-labelledby={`${uid}-near`}>
      <h2 className="ob-devs__label" id={`${uid}-near`}>
        {tr('附近设备')}<span className="devs__count">{nearby.length || ''}</span>
        <span className="ob-devs__tools"><RefreshButton onRefresh={actions.refresh} /></span>
      </h2>
      <div className="ob-devs__scroll">
        <ul className="addp__list">
          {near.map(({ item, phase }) => (
            <FoldItem key={item.id} phase={phase}>
              <NearbyRow peer={item} onConnect={() => { void actions.connect(item).catch(() => {}); }} ctx />
            </FoldItem>
          ))}
        </ul>
        {nearby.length === 0 && <p className="addp__empty"><Dots label={tr('正在搜索')} />{tr('正在搜索附近的设备')}</p>}
      </div>
    </section>
  );
}
