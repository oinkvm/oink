import { LANGUAGES, secondaryName, tr } from '../i18n';
import { useEffect, useId, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { Accordion, Button, ColorSwatches, DropdownMenu, MenuItem, Group, Icon, Input, Row, Segmented, Select, Switch, toast, useOink } from 'oink-ui';
import type { MotionMode, ThemeMode } from 'oink-ui';
import type { Platform } from '../window/DesktopWindow';
import { ACCENT_SWATCHES, DEFAULT_ACCENT } from './appearance';
import type { ReceivedFilesCleanup } from '../api/types';
import { api as backend, shell } from '../api/client';
import { agent, useAgent } from '../api/store';
import { ScreenPointerGains } from './ScreenPointerGains';
import { GainControl } from './GainControl';
import { KeyMapControl } from './KeyMap';
import { AudioQualityChart } from './AudioQualityChart';
import { LatencyChart } from './LatencyChart';
import { PermissionRow } from '../permissions/PermissionRow';
import { PermissionControl } from '../permissions/PermissionControl';
import { useGrant } from './useGrant';
import { UninstallControl } from './UninstallControl';
import { UpdateCheck, UpdatePreferences, manualUpdates } from './UpdatePreferences';
import { SHORTCUTS, ShortcutRecorder, spokenKeys } from './Shortcuts';
import type { ShortcutId } from './Shortcuts';
import type { LogLevel, PermissionId, Prefs, PrefsApi } from './prefs';
import { searchSections, sectionsFor, text } from './schema';
import { LayoutProfileFailure } from '../api/layoutProfiles';
import { profileErrorText } from '../arrange/snapshot';
import type { ItemMeta, SectionId } from './schema';
export type PermState = 'granted' | 'denied' | 'not_granted' | 'not_determined' | 'unknown' | 'unsupported' | 'not_required' | 'restart_required';
export interface SettingsJump {
    section: SectionId;
    item?: string;
    n: number;
}
function hl(s: string, q: string): ReactNode {
    const k = q.trim();
    if (!k)
        return s;
    const i = s.toLowerCase().indexOf(k.toLowerCase());
    if (i < 0)
        return s;
    return <>{s.slice(0, i)}<mark className="setp__hit">{s.slice(i, i + k.length)}</mark>{s.slice(i + k.length)}</>;
}
const formatSize = (b: number) => {
    const units = ['B', 'KB', 'MB', 'GB'];
    let i = 0;
    let v = b;
    while (v >= 1000 && i < units.length - 1) {
        v /= 1000;
        i++;
    }
    return `${i === 0 ? v : v.toFixed(1)} ${units[i]}`;
};
export function SettingsPage({ open, platform, api, capsIme, query, jump, onActive, version, perms, onGrant, onOpenPermissionSettings, onLog, onAbout, onBlocklist, layoutProfiles, onOnboarding, }: {
    open: boolean;
    platform: Platform;
    api: PrefsApi;
    capsIme: boolean | null;
    query: string;
    jump: SettingsJump | null;
    onActive: (id: SectionId) => void;
    version: string;
    upgrade: {
        available: boolean;
        next: string;
    };
    perms: Record<PermissionId, PermState>;
    onGrant: (id: PermissionId) => void | Promise<void>;
    onOpenPermissionSettings?: (id: PermissionId) => void | Promise<void>;
    onLog: (text: string, tone?: 'neutral' | 'info' | 'success' | 'warning' | 'danger') => void;
    onAbout: () => void;
    onBlocklist: () => void;
    /** Settings › Layout's profile list (create, rename, delete), owned by the app. */
    layoutProfiles?: ReactNode;
    onOnboarding?: () => void;
}) {
    const { prefs, set } = api;
    const [logDraft,setLogDraft] = useState(prefs.logMaxMb);
    useEffect(()=>setLogDraft(prefs.logMaxMb),[prefs.logMaxMb]);
    const agentState = useAgent();
    const { theme, setTheme, accent, setAccent, motion, setMotion } = useOink();
    const scroller = useRef<HTMLDivElement | null>(null);
    const uid = useId();
    // This device's own system-arrangement preference; hidden where the owner cannot write it at all.
    const arrangement = agentState.layoutProfiles?.devices.find(d => d.device === agentState.overview?.this_device.id);
    const [syncPending, setSyncPending] = useState(false);
    const applicable = (item: ItemMeta) => (item.id !== 'perm-network' || perms.network !== 'not_required') && (!['touchpadInject','macScrollFeel'].includes(item.id) || agentState.input?.contacts_injection_available === true)
        && (item.id !== 'syncSystemArrangement' || arrangement?.availability !== 'unsupported');
    const setSync = async (enabled: boolean, label: string) => {
        if (!arrangement) return;
        setSyncPending(true);
        try {
            agent.adoptProfiles(await backend.setSyncSystemArrangement({ request_id: crypto.randomUUID(), device: arrangement.device, expected_preference_token: arrangement.preference_token, enabled }));
            onLog(`${label}：${enabled ? tr('开') : tr('关')}`);
        } catch (e) {
            toast.danger({ title: tr('无法更改「{0}」', label), description: e instanceof LayoutProfileFailure ? profileErrorText(e.problem) : tr('不可用') });
        } finally { setSyncPending(false); }
    };
    const sections = sectionsFor(platform).map(s => ({ ...s, items: s.items.filter(applicable) }));
    const found = searchSections(platform, query);
    const shown = sections.filter((s) => {
        const keep = found.get(s.id);
        return keep === 'all' || (keep && s.items.some(i => keep.has(i.id)));
    });
    const lockUntil = useRef(0);
    const spy = () => {
        const el = scroller.current;
        if (!el || performance.now() < lockUntil.current)
            return;
        const secs = Array.from(el.querySelectorAll<HTMLElement>('[data-section]'));
        if (!secs.length)
            return;
        const top = el.getBoundingClientRect().top;
        const atEnd = el.scrollTop + el.clientHeight >= el.scrollHeight - 2;
        let cur = secs[0];
        for (const s of secs)
            if (s.getBoundingClientRect().top - top <= 32)
                cur = s;
        if (atEnd && el.scrollTop > 0)
            cur = secs[secs.length - 1];
        onActive(cur.dataset.section as SectionId);
    };
    useEffect(() => { scroller.current?.scrollTo({ top: 0 }); spy(); }, [query]);
    const [flash, setFlash] = useState<{
        id: string;
        n: number;
    } | null>(null);
    useEffect(() => {
        if (!jump || !scroller.current)
            return;
        const el = scroller.current;
        const target = el.querySelector<HTMLElement>(jump.item ? `[data-item="${jump.item}"]` : `[data-section="${jump.section}"]`);
        if (!target)
            return;
        const smooth = open;
        // Measure from the rows' own box: it carries the entry rise, so a
        // jump that opens the page lands where the row settles.
        const inner = target.closest<HTMLElement>('.setp__inner')!;
        const y = target.getBoundingClientRect().top - inner.getBoundingClientRect().top + inner.offsetTop - (jump.item ? 48 : 0);
        lockUntil.current = performance.now() + (smooth ? 900 : 0);
        el.scrollTo({ top: Math.max(0, y), behavior: smooth ? 'smooth' : 'auto' });
        onActive(jump.section);
        if (jump.item)
            setFlash({ id: jump.item, n: jump.n });
    }, [jump?.n]);
    const grant = useGrant(onGrant, onOpenPermissionSettings);
    useEffect(()=>{
        if(!open) return;
        let active=true;
        const before=agentState.receivedFiles;
        void backend.receivedFilesUsage().then(value=>{if(active && agent.get().receivedFiles===before) agent.adopt('receivedFiles',value);}).catch(()=>{});
        return ()=>{active=false;};
    },[open]);
    const received=agentState.receivedFiles;
    const [receivedSaving,setReceivedSaving]=useState(false);
    const openReceived=()=>{void backend.openReceivedFiles().catch(()=>agent.fail('unavailable'));};
    const setReceivedCleanup=async(value:ReceivedFilesCleanup)=>{
        if(receivedSaving) return;
        setReceivedSaving(true);
        try {await backend.setReceivedFilesCleanup(value);agent.adopt('receivedFiles',await backend.receivedFilesUsage());}
        catch {agent.fail('unavailable');}
        finally {setReceivedSaving(false);}
    };
    const [cacheBytes, setCacheBytes] = useState<number | null>(null);
    const [cacheNote, setCacheNote] = useState('');
    const [clearing, setClearing] = useState(false);
    useEffect(() => {
        if (open)
            void backend.cacheUsage().then(rows => {
                const self = agentState.overview?.this_device.id;
                const usage = rows.find(row => row.device === self)?.usage;
                if (usage) {
                    setCacheBytes(usage.total_bytes);
                    setCacheNote('');
                }
            }).catch(() => { });
    }, [open]);
    const clearCache = async () => {
        setClearing(true);
        try {
            const result = await backend.clearCache();
            setCacheBytes(result.result.usage.total_bytes);
            setCacheNote(tr('保留 {0}', formatSize(result.result.kept_bytes)));
        }
        catch {
            agent.fail('unavailable');
        }
        finally {
            setClearing(false);
        }
    };
    const [scError, setScError] = useState<{
        id: ShortcutId;
        text: string;
    } | null>(null);
    useEffect(() => {
        if (!scError)
            return;
        const t = window.setTimeout(() => setScError(null), 3000);
        return () => window.clearTimeout(t);
    }, [scError]);
    const [saving, setSaving] = useState(false);
    const openDir = () => { void shell.openDiagnosticsFolder().catch(() => agent.fail('unavailable')); };
    const saveBundle = async (minutes: number | null) => {
        setSaving(true);
        try {
            const result = await backend.exportDiagnostics(minutes);
            toast({ title: result.cutoff_complete === false || (result.missing_device_count ?? 0) > 0 ? tr('诊断包已保存，部分设备缺失') : tr('诊断包已保存'), action: { label: tr('打开'), onClick: openDir } });
        }
        catch {
            toast.danger({ title: tr('诊断包保存失败'), action: { label: tr('打开'), onClick: openDir } });
        }
        finally {
            setSaving(false);
        }
    };
    const toggle = (k: keyof Prefs, label: string) => (v: boolean) => { set(k, v as never); onLog(`${label}：${v ? tr('开') : tr('关')}`); };
    const control = (item: ItemMeta, label: string): {
        node: ReactNode;
        stack?: boolean;
        desc?: ReactNode;
        /** Show the control inside a collapsed disclosure under the label. */
        panel?: boolean;
    } => {
        const sw = (k: keyof Prefs) => ({ node: <Switch aria-label={label} checked={prefs[k] as boolean} onChange={toggle(k, label)}/> });
        switch (item.id) {
            case 'crashReports': {
                const description = text(item.desc, platform);
                const privacy = 'https://github.com/oinkvm/oink/blob/main/PRIVACY.md';
                return {
                    desc: <>{description}{' '}<a href={privacy} onClick={event => {
                        event.preventDefault();
                        void shell.openUrl(privacy).catch(() => agent.fail('unavailable'));
                    }}>{tr('隐私政策')}</a></>,
                    node: <Switch aria-label={label} checked={prefs.crashReports} onChange={toggle('crashReports', label)}/>,
                };
            }
            case 'launchAtLogin': return { desc: api.launchStatus?.state === 'requires_approval' ? tr('需要在系统设置中批准登录项') : undefined, node:<span className="setp__btns">{api.launchStatus?.state === 'requires_approval' && <Button size="sm" onClick={()=>{void backend.openLaunchAtLoginSettings().catch(()=>agent.fail('unavailable'));}}>{tr('打开设置')}</Button>}<Switch aria-label={label} checked={prefs.launchAtLogin} onChange={toggle('launchAtLogin',label)} disabled={api.launchStatus?.state === 'unavailable'}/></span> };
            case 'showInMenuBar':
            case 'confirmQuit':
            case 'escStop':
            case 'scrollInvert':
            case 'forwardInjectedInput':
            case 'keepAwake':
            case 'touchpadInject':
            case 'pointerTrail':
            case 'keepLocalPlayback':
            case 'diagnostics':
            case 'clipReceive':
            case 'clipSend':
                return sw(item.id);
            case 'clipText':
            case 'clipRich':
            case 'clipImage':
            case 'clipFiles':
            case 'clipPrivate':
                return { node: <Switch aria-label={label} checked={prefs[item.id]} onChange={toggle(item.id,label)} disabled={!prefs.clipSend}/> };
            case 'macScrollFeel': return { node: <Switch aria-label={label} checked={prefs.macScrollFeel} onChange={toggle('macScrollFeel',label)} disabled={!prefs.touchpadInject}/> };
            case 'hideSponsor': return { node: <Switch aria-label={label} checked={api.presentation?.hide_sponsor ?? false} onChange={v => api.savePresentation({ hide_sponsor: v })}/> };
            case 'onboarding': return { node: <Button size="sm" onClick={onOnboarding}>{tr('显示')}</Button> };
            case 'uninstall': return { node: <UninstallControl/> };
            case 'blocklist':
                return { node: <Button size="sm" onClick={onBlocklist}>{tr('管理')}</Button> };
            case 'syncSystemArrangement': {
                const ready = agentState.layoutProfiles?.readiness === 'ready' && arrangement?.availability === 'ready';
                return {
                    desc: arrangement?.availability === 'unavailable' ? <>{text(item.desc, platform)} · {tr('暂时读不到系统屏幕排列')}</> : undefined,
                    node: <Switch aria-label={label} checked={arrangement?.sync_system_arrangement ?? false} disabled={!ready || syncPending} onChange={v => { void setSync(v, label); }}/>,
                };
            }
            case 'manageLayouts':
                return { node: layoutProfiles ?? null, stack: true };
            case 'receivedFiles': return {node:<>
                <span className="setp__size">{received ? `${received.incomplete ? '≥ ' : ''}${formatSize(received.materialized_bytes)}` : '—'}</span>
                <Button size="sm" leading={<Icon name="folder"/>} onClick={openReceived}>{tr('管理')}</Button>
            </>};
            case 'receivedCleanup': return {node:<Select size="sm" aria-label={label} value={received?.cleanup ?? 'never'} disabled={!received || receivedSaving} onChange={value=>{void setReceivedCleanup(value as ReceivedFilesCleanup);}} options={[
                {value:'days7',label:received?.time_basis==='last_used' ? tr('7 天未使用') : tr('收到 7 天后')},
                {value:'days30',label:received?.time_basis==='last_used' ? tr('30 天未使用') : tr('收到 30 天后')},
                {value:'never',label:tr('从不')},
            ]}/>};
            case 'cache':
                return {
                    desc: cacheNote ? <>{text(item.desc, platform)} · {cacheNote}</> : undefined,
                    node: (<span className="setp__btns">
              <span className="setp__size">{cacheBytes === null ? '—' : cacheBytes === 0 ? tr('没有缓存') : formatSize(cacheBytes)}</span>
              <Button size="sm" loading={clearing} disabled={cacheBytes === 0} onClick={clearCache}>{tr('清空缓存')}</Button>
            </span>),
                };
            case 'version':
                return {
                    desc: <span className="setp__mono">v{version}</span>,
                    node: <UpdateCheck status={agentState.update} open={open}/>,
                };
            case 'updates': return { desc: manualUpdates(agentState.update) ? tr('此版本需要手动下载并安装更新。') : undefined, node: <UpdatePreferences status={agentState.update} label={label}/> };
            case 'about':
                return { node: <Button size="sm" onClick={onAbout}>{tr('查看')}</Button> };
            case 'language':
                return {
                    node: (<Select size="sm" aria-label={label} value={prefs.language} onChange={(v) => { set('language', v); onLog(tr('语言 → {0}', v === 'auto' ? tr('自动') : LANGUAGES.find((l) => l.id === v)?.autonym)); }} options={[
                            { value: 'auto', get label() {
                                    return tr('自动');
                                }, meta: tr('跟随系统') },
                            { get label() {
                                    return tr('语言');
                                }, options: LANGUAGES.map((l) => ({ value: l.id, label: <span lang={l.id}>{l.autonym}</span>, textValue: l.autonym, meta: secondaryName(l) })) },
                        ]}/>),
                };
            case 'accent':
                return { node: <ColorSwatches aria-label={label} size="sm" colors={ACCENT_SWATCHES} value={accent} onChange={(v) => { setAccent(v as typeof accent); void api.saveLocal({ theme_accent: { light: ACCENT_SWATCHES.find(c => c.value === v)?.color ?? DEFAULT_ACCENT.color, dark: ACCENT_SWATCHES.find(c => c.value === v)?.dark ?? DEFAULT_ACCENT.dark } }).catch(() => {setAccent(accent);agent.fail('unavailable');}); onLog(tr('主题色 → {0}', ACCENT_SWATCHES.find((c) => c.value === v)?.label)); }}/> };
            case 'theme':
                return {
                    node: (<Segmented<ThemeMode> size="sm" aria-label={label} value={theme} onChange={(v) => { setTheme(v); void api.saveLocal({ theme_mode: v }).catch(() => {setTheme(theme);agent.fail('unavailable');}); onLog(tr('模式 → {0}', { system: tr('跟随系统'), light: tr('浅色'), dark: tr('深色') }[v])); }} options={[
                            { value: 'system', get label() {
                                    return tr('跟随系统');
                                }, icon: <Icon name="monitor"/> },
                            { value: 'light', get label() {
                                    return tr('浅色');
                                }, icon: <Icon name="sun"/> },
                            { value: 'dark', get label() {
                                    return tr('深色');
                                }, icon: <Icon name="moon"/> },
                        ]}/>),
                };
            case 'motion':
                return {
                    node: (<Segmented<MotionMode> size="sm" aria-label={label} value={motion} onChange={(v) => { setMotion(v); api.savePresentation({ motion: v }); onLog(tr('减少动画效果 → {0}', { auto: tr('跟随系统'), reduced: tr('开启'), full: tr('state|关闭') }[v])); }} options={[{ value: 'auto', get label() {
                                return tr('跟随系统');
                            } }, { value: 'reduced', get label() {
                                return tr('开启');
                            } }, { value: 'full', get label() {
                                return tr('state|关闭');
                            } }]}/>),
                };
            case 'keymap':
                return { desc: api.hasOverrides ? tr('有按设备设置的覆盖') : undefined, node: <KeyMapControl platform={platform} api={api} capsIme={capsIme} label={label} onLog={onLog}/>, stack: true };
            case 'pollLimit':
                return {
                    node: (<Segmented<Prefs['pollLimit']> size="sm" aria-label={label} value={prefs.pollLimit} onChange={(v) => { set('pollLimit', v); onLog(tr('指针事件频率上限 → {0}', v === '125' ? '125 Hz' : tr('不限制'))); }} options={[{ value: '125', label: '125 Hz' }, { value: 'none', get label() {
                                return tr('不限制');
                            } }]}/>),
                };
            case 'screenPointerGains': return { node: <ScreenPointerGains/>, panel: true };
            case 'pointerGain':
                return { node: <GainControl label={label} value={prefs.pointerGain} onChange={(v) => set('pointerGain', v)} onCommit={(v) => onLog(tr('被控时指针速度增益 → {0}x', v.toFixed(2)))}/> };
            case 'audioDevice': {
                const list = agentState.audio?.devices.find(d => d.device.map(v => v.toString(16).padStart(2, '0')).join('') === agentState.overview?.this_device.id)?.outputs ?? [];
                return {
                    node: (<Select size="sm" aria-label={label} value={prefs.audioDevice} onChange={(v) => { set('audioDevice', v); onLog(tr('音频输出设备 → {0}', v === 'default' ? tr('默认输出设备') : v)); }} options={[
                            { value: 'default', get label() {
                                    return tr('默认输出设备');
                                }, meta: list.find(d => d.is_default)?.name },
                            { get label() {
                                    return tr('输出设备');
                                }, options: list.map((d) => ({ value: d.id, label: d.name })) },
                        ]}/>),
                };
            }
            case 'notificationLevel':
                return { node: <Select size="sm" aria-label={label} value={prefs.notificationLevel} onChange={v => set('notificationLevel', v as Prefs['notificationLevel'])} options={[
                    {value:'disabled',label:tr('notifications|禁用')}, {value:'minimal',label:tr('notifications|精简')},
                    {value:'default',label:tr('notifications|默认')}, {value:'verbose',label:tr('notifications|详细')},
                ]}/> };
            case 'logLevel':
                return {
                    node: (<Select size="sm" aria-label={label} value={prefs.logLevel} onChange={(v) => { set('logLevel', v as LogLevel); onLog(tr('日志级别 → {0}', v)); }} options={[
                            { value: 'error', get label() {
                                    return tr('错误');
                                }, meta: 'error' },
                            { value: 'warn', get label() {
                                    return tr('警告');
                                }, meta: 'warn' },
                            { value: 'info', get label() {
                                    return tr('信息');
                                }, meta: 'info' },
                            { value: 'debug', get label() {
                                    return tr('调试');
                                }, meta: 'debug' },
                            { value: 'trace', get label() {
                                    return tr('跟踪');
                                }, meta: 'trace' },
                        ]}/>),
                };
            case 'logMaxMb':
                return { node: <Input size="sm" className="setp__mb" aria-label={label} inputMode="numeric" suffix="MB" value={logDraft} invalid={!/^\d+$/.test(logDraft) || Number(logDraft) < 1} onChange={v => setLogDraft(v.replace(/\D/g,''))} onBlur={()=>{ if(/^\d+$/.test(logDraft) && Number(logDraft) >= 1) set('logMaxMb',logDraft); }}/> };
            case 'bundle':
                return {
                    node: (<span className="setp__btns">
                      <Button size="sm" leading={<Icon name="folder"/>} onClick={openDir}>{tr('打开')}</Button>
                      <DropdownMenu placement="bottom-end" matchTriggerWidth={false}
                        trigger={<Button size="sm" loading={saving} disabled={saving} trailing={<Icon name="chevron-down"/>}>{tr('保存')}</Button>}>
                        <MenuItem onSelect={() => { void saveBundle(30); }}>{tr('最近 30 分钟')}</MenuItem>
                        <MenuItem onSelect={() => { void saveBundle(120); }}>{tr('最近 2 小时')}</MenuItem>
                        <MenuItem onSelect={() => { void saveBundle(null); }}>{tr('全部')}</MenuItem>
                      </DropdownMenu>
                    </span>),
                };
            case 'audioQuality':
                return {node: <AudioQualityChart active={open}/>, stack:true};
            case 'latency':
                return { node: <LatencyChart active={open}/>, stack: true };
        }
        if (item.id.startsWith('sc-')) {
            const id = item.id.slice(3) as ShortcutId;
            const sc = prefs.shortcuts[id];
            const clash = sc.on && sc.keys ? SHORTCUTS.find((o) => o.id !== id && prefs.shortcuts[o.id].on && prefs.shortcuts[o.id].keys === sc.keys) : undefined;
            const err = scError?.id === id ? scError.text : clash ? tr('和「{0}」的快捷键重复', clash.label) : null;
            const setSc = (patch: Partial<typeof sc>) => set('shortcuts', { ...prefs.shortcuts, [id]: { ...sc, ...patch } });
            return {
                desc: err
                    ? <span className="setp__sc-err"><Icon name="alert"/>{err}</span>
                    : hl(text(item.desc, platform), query),
                node: (<span className="setp__sc">
            <ShortcutRecorder platform={platform} label={label} value={sc.keys} disabled={!sc.on} invalid={!!clash} onChange={(v) => { setScError(null); setSc({ keys: v }); onLog(tr('快捷键「{0}」→ {1}', label, spokenKeys(platform, v))); }} onReject={(reason) => setScError({ id, text: reason })}/>
            <Switch aria-label={tr('启用「{0}」快捷键', label)} checked={sc.on} onChange={(v) => { setSc({ on: v }); onLog(tr('快捷键「{0}」：{1}', label, v ? tr('开') : tr('关'))); }}/>
          </span>),
            };
        }
        if (item.id.startsWith('perm-')) {
            const id = item.id.slice(5) as PermissionId;
            return { desc: id === 'network' ? tr('系统设置 › 隐私与安全性 › 本地网络') : id === 'input' && perms.accessibility === 'granted' && perms.input === 'granted' ? tr('辅助功能已包含输入监控，无需重复授权') : undefined,
                node: <PermissionControl id={id} state={perms[id]} grant={grant}/> };
        }
        return { node: null };
    };
    const empty = shown.length === 0;
    return (<div className="setp ok-scroll" data-open={open ? '' : undefined} ref={(el) => { scroller.current = el; if (el)
        el.inert = !open; }} onScroll={spy} aria-hidden={!open || undefined}>
      <div className="setp__inner">
        {shown.map((s) => {
            const keep = found.get(s.id)!;
            const items = s.items.filter((i) => keep === 'all' || keep.has(i.id));
            return (<section key={s.id} className="setp__sec" data-section={s.id} aria-labelledby={`${uid}-${s.id}`}>
              <h2 className="setp__title" id={`${uid}-${s.id}`}>{hl(s.title, query)}</h2>
              {s.id === 'permissions' && <p className="setp__lead">{tr('Oink 需要这些系统权限。没有授权的，对应的共享功能不可用。')}</p>}
              <Group className="setp__card" aria-busy={api.pending}>
                {items.length === 0 && <p className="setp__none">{tr('暂无可调整的设置')}</p>}
                {items.map((i) => {
                    const label = text(i.label, platform);
                    const c = control(i, label);
                    const desc = c.desc ?? (i.desc ? hl(text(i.desc, platform), query) : undefined);
                    const node = api.canEdit(i.id) ? c.node : <span className="setp__note">{tr('不可用')}</span>;
                    const marks = { 'data-item': i.id, 'data-flash': flash?.id === i.id ? flash.n : undefined, onAnimationEnd: () => setFlash(null) };
                    if (c.panel)
                        return (<Accordion key={i.id} {...marks} variant="plain" headingLevel={3} className="setp__row setp__acc" items={[{ value: i.id, content: node,
                            title: <span className="ok-row__text"><span className="ok-row__label">{hl(label, query)}</span>{desc && <span className="ok-row__desc">{desc}</span>}</span> }]}/>);
                    const ItemRow = i.id.startsWith('perm-') ? PermissionRow : Row;
                    return (<ItemRow key={i.id} {...marks} className="setp__row" label={hl(label, query)} description={desc} stack={c.stack}>
                      {node}
                    </ItemRow>);
                })}
              </Group>
            </section>);
        })}
        {empty && (<div className="setp__empty">
            <Icon name="search"/>
            <p>{tr('没有找到与“{0}”相关的设置', query.trim())}</p>
          </div>)}
      </div>
    </div>);
}
