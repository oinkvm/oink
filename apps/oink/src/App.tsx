import { LayoutProjectionPreview } from './arrange/projection';
import { useEffect, useId, useMemo, useRef, useState } from 'react';
import { Button, Confirmer, Icon, OinkProvider, Toaster, Tooltip, VisuallyHidden, mergeRefs, speed } from 'oink-ui';
import { ApiFailure, api, shell } from './api/client';
import { agent, start, useAgent } from './api/store';
import { inTauri } from './api/transport';
import { presentCards, resourceAction } from './devices/project';
import { DeviceItem, DeviceList, hasExit } from './devices/DeviceList';
import { PauseGlyph } from './devices/icons';
import { PeekScope } from './devices/peek';
import { SideContextMenu, type SideContextMenuProps } from './devices/DeviceMenu';
import { toggleShare, toggleThird } from './devices/data';
import type { ShareKind, ShareValue } from './devices/data';
import type { Device } from './devices/types';
import type { SpeakerAction } from './devices/VolumeRow';
import { mergeVisibleGroups } from './arrange/groups';
import { Arrangement } from './arrange/Arrangement';
import type { Placement } from './api/types';
import type { Screen } from './arrange/geometry';
import { useHistory } from './arrange/useHistory';
import { renderLayout, layoutPlacements, presentScreens, replayPlacements, visibleScreens } from './arrange/adapter';
import { DesktopWindow } from './window/DesktopWindow';
import { ResizeEdges } from './window/ResizeEdges';
import { QuitDialog } from './window/QuitDialog';
import { AboutWindow } from './window/AboutWindow';
import { UpgradeButton, UpgradeDialog } from './window/Upgrade';
import { PromoArea } from './window/PromoArea';
import { StatusNotices } from './window/StatusNotices';
import { observePanel } from './window/panel';
import { revealWhenPainted } from './window/firstPaint';
import { Onboarding, OnboardingLanguage } from './onboarding/Onboarding';
import { LeaveDialog } from './devices/LeaveDialog';
import { AddressDialog } from './devices/AddressDialog';
import { useAddressEditor } from './devices/useAddressEditor';
import { AddHead, AddPanel, RequestRow, RequestsGroup, SelfInfo, fromRequestPeekId } from './pairing/AddPanel';
import { PairDialog } from './pairing/PairDialog';
import { usePairing } from './pairing/usePairing';
import { SettingsNav } from './settings/SettingsNav';
import { SettingsPage, type SettingsJump, type PermState } from './settings/SettingsPage';
import { BlockListDialog } from './settings/BlockListDialog';
import { usePreferences } from './settings/model';
import { DEFAULT_ACCENT } from './settings/appearance';
import { AppearanceSync } from './settings/AppearanceSync';
import type { PermissionId } from './settings/prefs';
import type { SectionId } from './settings/schema';
import { problemText, tr, useLanguage } from './i18n';
import { version } from './version';
import './settings/settings.css';
async function windowAction(action: 'minimize' | 'toggleMaximize' | 'close') {
    if (!inTauri)
        return;
    const { getCurrentWindow } = await import('@tauri-apps/api/window');
    await getCurrentWindow()[action]();
}
const samePlacements = (a: Placement[], b: Placement[]) => JSON.stringify(a) === JSON.stringify(b);
export function App() {
    const state = useAgent();
    const language = useLanguage();
    const prefs = usePreferences();
    const view = new URLSearchParams(location.search).get('view') ?? 'main';
    const pairing = usePairing(view === 'main');
    const platform = state.boot?.platform === 'windows' ? 'win' : 'mac';
    const devices = presentCards(state.cards, state.pending);
    const self = devices.find(d => d.group === 'self');
    const [navOpen, setNavOpen] = useState(true);
    const [maximized,setMaximized] = useState(false);
    const [windowActive,setWindowActive] = useState(true);
    const [navMode, setNavModeState] = useState<'devices' | 'add' | 'settings'>('devices');
    const [navSwitched,setNavSwitched] = useState(false);
    const [settingsMounted,setSettingsMounted] = useState(false);
    const [screenOnly,setScreenOnly] = useState(false);
    const [peekId,setPeekId] = useState<string|null>(null);
    const [growIds,setGrowIds] = useState<ReadonlySet<string>>(new Set());
    const [query, setQuery] = useState('');
    const [section, setSection] = useState<SectionId>('general');
    const [jump, setJump] = useState<SettingsJump | null>(null);
    const [selectedDevice, setSelectedDevice] = useState<string | null>(null);
    const [selected, setSelected] = useState<string | null>(null);
    const [quitAsk, setQuitAsk] = useState(false);
    const [onboard, setOnboard] = useState<'on' | 'leaving' | 'off' | null>(null);
    const [onboardKey, setOnboardKey] = useState(0);
    const onboardingBusy = useRef(false);
    const onboardTimer = useRef(0);
    const onboarding = view === 'main' && (onboard === null ? !!state.overview && !state.overview.onboarding_done : onboard !== 'off');
    useEffect(() => {
        if (view === 'main' && state.overview && onboard === null) setOnboard(state.overview.onboarding_done ? 'off' : 'on');
    }, [view, state.overview, onboard]);
    useEffect(() => () => window.clearTimeout(onboardTimer.current), []);
    const finishOnboarding = async () => {
        if (onboardingBusy.current) return;
        onboardingBusy.current = true;
        try {
            await api.completeOnboarding();
            setOnboard('leaving');
            onboardTimer.current = window.setTimeout(() => setOnboard('off'), 700 * Math.max(speed(), 0));
            await agent.query('overview', api.overview);
            return true;
        } catch { agent.fail('unavailable'); return false; }
        finally { onboardingBusy.current = false; }
    };
    const replayOnboarding = () => {
        window.clearTimeout(onboardTimer.current);
        pairing.actions.dismiss();
        setOnboardKey(k => k + 1);
        setOnboard('on');
    };
    const [blockedOpen, setBlockedOpen] = useState(false);
    const [unblocking, setUnblocking] = useState<string[]>([]);
    const [blockError, setBlockError] = useState<string | null>(null);
    const [leaveId, setLeaveId] = useState<string | null>(null);
    const address = useAddressEditor();
    const [contextOpen, setContextOpen] = useState(false);
    const [winEl, setWinEl] = useState<HTMLDivElement | null>(null);
    const [savingLayout, setSavingLayout] = useState(false);
    const canvas = useRef<HTMLDivElement>(null);
    const hint = useId();
    const history = useHistory<Placement[]>(() => [], { equals: samePlacements });
    const screens = presentScreens(state.layout ? renderLayout(state.layout,state.wallpapers,history.value) : [], devices);
    // A device-local editing preference; only manual canvas edits compact.
    const compactLayout = prefs.presentation.compact_layout;
    const setNavMode = (mode: typeof navMode) => {
        if (mode === navMode) return;
        if (mode === 'settings') setSettingsMounted(true);
        setNavSwitched(true); setNavModeState(mode);
        winEl?.querySelector('.win__side-clip')?.scrollTo({top:0});
    };
    const selectDevice = (id: string) => {
        if (selectedDevice === id) { setSelectedDevice(null); setSelected(null); return; }
        if (devices.find(d => d.id === id)?.group === 'offline') return;
        setSelectedDevice(id); setScreenOnly(false);
        setSelected(screens.find(s => s.device === id)?.id ?? null);
    };
    const selectScreen = (id: string | null) => {
        setSelected(id); setScreenOnly(true);
        const screen = screens.find(s => s.id === id);
        const device = devices.find(d => d.id === screen?.device);
        setSelectedDevice(device && device.group !== 'offline' ? device.id : null);
    };
    const selectedGone = !!selectedDevice && !devices.some(d => d.id === selectedDevice && d.group !== 'offline');
    useEffect(() => { if (selectedGone) { setSelectedDevice(null); setSelected(null); } }, [selectedGone]);

    useEffect(() => { void start(view); }, []);
    useEffect(() => view !== 'panel' && inTauri ? revealWhenPainted(() => { void shell.ready().catch(() => agent.fail('unavailable')); }) : undefined, [view]);
    useEffect(() => {
        if(!inTauri)return;
        let stopped=false;const stops:Array<()=>void>=[];
        void import('@tauri-apps/api/window').then(async ({getCurrentWindow})=>{
            const window=getCurrentWindow();
            // macOS zoom keeps a floating shell; fullscreen and Windows
            // maximization fill the desktop without a gutter or corners.
            const update=async()=>setMaximized(await window.isFullscreen() || (await window.isMaximized() && state.boot?.platform === 'windows'));
            await update();
            const resized=await window.onResized(()=>{void update();});
            const focused=await window.onFocusChanged(event=>setWindowActive(event.payload));
            if(stopped){resized();focused();}else stops.push(resized,focused);
        });
        return()=>{stopped=true;stops.forEach(stop=>stop());};
    },[state.boot?.platform]);

    useEffect(() => {
        if (!state.layout)
            return;
        history.sync(layoutPlacements(state.layout,[]));
    }, [state.layout]);
    useEffect(() => {
        if (view !== 'panel' || !winEl || !self || !prefs.local) return;
        return observePanel(winEl);
    }, [view,winEl,self?.id,!!prefs.local]);
    useEffect(() => {
        if (state.boot?.platform === 'windows' && prefs.local) void shell.setStatusIcon(prefs.local.show_status_icon).catch(() => agent.fail('unavailable'));
    }, [state.boot?.platform,prefs.local?.show_status_icon]);
    useEffect(() => {if(prefs.local)void shell.setLogLevel(prefs.local.log_level).catch(()=>agent.fail('unavailable'));},[prefs.local?.log_level]);
    useEffect(() => {if(prefs.local)void shell.setLanguage(prefs.local.ui_language).catch(()=>agent.fail('unavailable'));},[prefs.local?.ui_language]);
    useEffect(() => {void shell.liveViews({stats:false,group_quality:view==='main'||view==='panel',audio_quality:view==='main'&&navMode==='settings'&&(section==='diagnostics'||query.trim().length>0),diagnostics:false}).catch(()=>agent.fail('unavailable'));},[view,navMode,section,query]);
    const fail = (e: unknown) => agent.fail(e instanceof ApiFailure ? e.problem.code : 'unavailable');
    const permissionPage = () => { setNavMode('settings'); setNavOpen(true); setJump({ section: 'permissions', n: Date.now() }); };
    const clicks = useRef(new Map<string, { timer: ReturnType<typeof setTimeout>; requestId: string }>());
    useEffect(() => () => { for (const click of clicks.current.values()) clearTimeout(click.timer); }, []);
    const invoke = async (id: string, resource: string, desired: string | boolean | number, delaySingle = false) => {
        const card = state.cards?.cards.find(c => c.device === id);
        if (resource === 'screen' && desired === 'locked') {
            try {
                await api.lockFocusTo(id);
            }
            catch (e) {
                fail(e);
            }
            return;
        }
        if (!card)
            return;
        const action = resourceAction(card, resource, desired);
        if (action?.kind === 'open_permissions') {
            permissionPage();
            return;
        }
        if (action?.kind !== 'invoke') {
            agent.fail(action?.kind === 'unavailable' ? action.reason : 'unsupported');
            return;
        }
        const key = `${id}:${resource}`;
        const scheduled = clicks.current.get(key);
        if (scheduled) { clearTimeout(scheduled.timer); clicks.current.delete(key); agent.reject(scheduled.requestId, ''); }
        const requestId = crypto.randomUUID();
        if (!agent.pending({ requestId, device: id, resource, desired })) return;
        const send = async () => {
            clicks.current.delete(key);
            try {
                const result = await api.invokeCard(requestId, action.token);
                agent.controlReply(result);
            } catch { agent.reject(requestId, 'outcome_unknown'); void agent.query('cards', api.deviceCards).catch(fail); }
        };
        // Preserve instant feedback while coalescing a double click into one absolute owner command.
        if (delaySingle) clicks.current.set(key, { timer: setTimeout(() => { void send(); }, 600), requestId });
        else await send();
    };
    const changeShare = (id: string, kind: ShareKind, desired: ShareValue) => { void invoke(id, kind, desired); };
    const single = useRef<{
        id: string;
        kind: ShareKind;
        before: Device[];
        at: number;
    } | null>(null);
    const toggle = (id: string, kind: ShareKind, mode: 'single' | 'double') => {
        const before = devices;
        const d = before.find(d => d.id === id);
        if (!d)
            return;
        const next = mode === 'double' ? toggleThird(single.current?.id === id && single.current.kind === kind && Date.now() - single.current.at < 600 ? single.current.before : before, id, kind)?.find(d => d.id === id) : toggleShare(d, kind);
        single.current = { id, kind, before, at: Date.now() };
        if (!next) {
            permissionPage();
            return;
        }
        void invoke(id, kind, (kind === 'speaker' ? next.speaker.mode : next[kind]) as ShareValue, mode === 'single' && (kind === 'keyboard' || kind === 'speaker'));
    };
    const speaker = async (id: string, action: SpeakerAction) => {
        const exit = state.cards?.exitVolume;
        const card = state.cards?.cards.find(c => c.device === id);
        const forwarding = card?.status?.speaker.forwarding;
        const isExit = exit?.receiverDevice === id;
        const token = isExit ? exit.versionToken : card?.status?.speaker.forwardingControlsToken;
        const volume = isExit ? exit.volumePercent : forwarding?.volumePercent;
        const muted = isExit ? exit.muted : forwarding?.muted;
        if (!token || volume == null || muted == null || (isExit && !exit.canSet)) {
            agent.fail('unsupported');
            return;
        }
        const requestId = crypto.randomUUID();
        if (!agent.pending({ requestId, device: id, resource: 'speaker_controls', desired: action.type === 'mute' ? !muted : action.value }))
            return;
        try {
            const apply = isExit ? api.setExitVolume : api.setForwardingControls;
            const result = await apply(requestId, token, action.type === 'volume' ? action.value : volume, action.type === 'mute' ? !muted : false);
            agent.controlReply(result);
        }
        catch {
            agent.reject(requestId, 'outcome_unknown');
            void agent.query('cards', api.deviceCards).catch(fail);
        }
    };
    const persistPlacements = async (placements: Placement[], checkpoint = history.checkpoint()) => {
        setSavingLayout(true);
        try {
            await agent.query('layout', () => api.saveLayout(placements));
        }
        catch (e) {
            history.restore(checkpoint);
            fail(e);
        }
        finally {
            setSavingLayout(false);
        }
    };
    const [projection] = useState(() => new LayoutProjectionPreview(api.previewLayoutProjection));
    useEffect(() => () => projection.close(), [projection]);
    const previewScreens = (next: Screen[] | null) => {
        if (!state.layout || !next) {
            projection.end();
            return;
        }
        projection.update(layoutPlacements(state.layout, mergeVisibleGroups(screens, next)));
    };
    const saveScreens = (next: Screen[], label: string, merge?: string) => {
        if (!state.layout || savingLayout) return;
        const checkpoint = history.checkpoint();
        const placements = layoutPlacements(state.layout, next);
        history.commit(placements, label, merge);
        return persistPlacements(placements, checkpoint);
    };
    const travel = (direction: 'undo' | 'redo') => {
        if (savingLayout || !state.layout) return;
        const checkpoint = history.checkpoint();
        const move = history[direction]();
        // The history transition already happened. Recommitting a rendered
        // round trip could round an origin and discard the redo stack.
        if (move) void persistPlacements(replayPlacements(state.layout, move.value), checkpoint);
    };
    const resetLayout = async () => {
        if (savingLayout) return;
        const checkpoint = history.checkpoint(); setSavingLayout(true);
        try {
            const layout = await api.resetInitialLayout();
            history.commit(layoutPlacements(layout,[]), tr('重置排列'));
            agent.adopt('layout',layout);
        } catch (e) { history.restore(checkpoint); fail(e); }
        finally { setSavingLayout(false); }
    };
    useEffect(() => {
        const key = (event: KeyboardEvent) => {
            if (onboarding || event.defaultPrevented || event.isComposing || navMode === 'settings' || pairing.dialog.open || quitAsk || blockedOpen || leaveId || address.device || canvas.current?.querySelector('[data-interacting]') || !(event.metaKey || event.ctrlKey) || event.altKey || (event.target instanceof HTMLElement && event.target.closest('input,textarea,[contenteditable=true]'))) return;
            if (event.key.toLowerCase() === 'z') { event.preventDefault(); travel(event.shiftKey ? 'redo' : 'undo'); }
            else if (event.ctrlKey && event.key.toLowerCase() === 'y') { event.preventDefault(); travel('redo'); }
        };
        document.addEventListener('keydown', key); return () => document.removeEventListener('keydown', key);
    });
    // One-shot refreshes on navigation/activation; ongoing observations are backend events.
    useEffect(() => {
        if (view !== 'main') return;
        const refresh = () => { if (document.visibilityState !== 'hidden') void agent.query('permissions', api.permissions).catch(() => {}); };
        refresh();
        window.addEventListener('focus', refresh);
        document.addEventListener('visibilitychange', refresh);
        return () => { window.removeEventListener('focus', refresh); document.removeEventListener('visibilitychange', refresh); };
    }, [view, navMode, windowActive]);
    const grant = async (id: PermissionId) => {
        if (id === 'files')
            await api.openFileProviderEnable();
        else if (id === 'audio')
            await api.requestPermission('audio_capture');
        else
            await api.requestPermission(id === 'input' ? 'input_monitoring' : id === 'network' ? 'local_network' : id);
    };
    const openPermissionSettings = async (id: PermissionId) => {
        if (id === 'files') await api.openFileProviderEnable();
        else await api.openPermissionSettings(id === 'input' ? 'input_monitoring' : id === 'network' ? 'local_network' : id === 'audio' ? 'audio_capture' : 'accessibility');
    };
    const menu: Omit<SideContextMenuProps, 'children'> = {
        devices, requests: pairing.requests, nearby: pairing.nearby, onShare: changeShare,
        onGameMode: (id, mode) => { void invoke(id, 'game', mode); }, onSpeaker: (id, action) => { void speaker(id, action); },
        onPause: (id, paused) => { void invoke(id, 'pause', paused); }, onLeave: setLeaveId,
        onEditAddress: id => { const device=devices.find(d=>d.id===id); if(device)void address.open(device); },
        onShowWindow: () => { void shell.open('main').catch(fail); }, onAbout: () => { void shell.open('about').catch(fail); },
        onQuit: () => { void shell.quit().catch(fail); },
        onAccept: req => { if(view==='panel')void shell.open('main').then(()=>pairing.actions.accept(req)).catch(fail);else pairing.actions.accept(req); }, onReject: req => { void pairing.actions.reject(req).catch(fail); },
        onPair: p => { void pairing.actions.connect(p).catch(fail); }, onBlock: p => { void pairing.actions.block(p).catch(fail); },
        onOpenChange: setContextOpen,
    };
    useEffect(()=>{
        if(!peekId)return;const list=(['self','online','offline'] as const).flatMap(group=>devices.filter(d=>d.group===group).map(d=>d.id));
        const rank=(id:string)=>fromRequestPeekId(id)?-1:list.indexOf(id);
        setGrowIds(previous=>{const next=new Set([...previous].filter(id=>rank(id)<=rank(peekId)));if(peekId===selectedDevice)next.add(peekId);return next.size===previous.size&&[...next].every(id=>previous.has(id))?previous:next;});
    },[peekId,selectedDevice]);
    const visible = useMemo(()=>visibleScreens(screens, state.layout, devices),[screens,state.cards,state.pending,state.layout]);
    useEffect(()=>{if(selected&&!visible.some(s=>s.id===selected))setSelected(null);},[visible,selected]);
    const permissions: Record<PermissionId, PermState> = {
        accessibility: 'unknown', input: 'unknown', network: 'unknown', files: 'unknown', audio: 'unknown',
    };
    for (const [id, kind] of [['accessibility','accessibility'],['input','input_monitoring'],['network','local_network']] as const) {
        const permission = state.permissions.find(p => p.kind === kind)?.state;
        permissions[id] = permission === 'Granted' ? 'granted' : permission === 'NotApplicable' ? 'not_required' : permission === 'NotGranted' ? 'not_granted' : permission === 'NotDetermined' ? 'not_determined' : permission === 'Denied' ? 'denied' : 'unknown';
    }
    // Only show this row after the backend has advertised its applicability.
    // Otherwise macOS 14 can briefly show it while the initial query is pending.
    if (!state.permissions.some(p => p.kind === 'local_network')) permissions.network = 'not_required';
    if (state.overview?.alerts.some(a => a.kind.type === 'capture_unavailable')) {
        if (permissions.accessibility === 'granted') permissions.accessibility = 'restart_required';
        if (permissions.input === 'granted') permissions.input = 'restart_required';
    }
    const fileState = state.fileProvider?.state;
    if (fileState) permissions.files = fileState === 'available' ? 'granted' : fileState === 'unsupported' ? 'unsupported' : fileState === 'checking' ? 'unknown' : fileState === 'error' ? 'restart_required' : 'denied';
    const capturePermission = state.audio?.capture_permission;
    if (capturePermission) permissions.audio = capturePermission === 'authorized' ? 'granted' : capturePermission === 'not_required' ? 'not_required' : capturePermission === 'not_requested' ? 'not_determined' : capturePermission === 'unknown' ? 'unknown' : 'denied';
    const body = view === 'about' ? <AboutWindow platform={platform} open front build={state.boot?.build} version={state.boot?.version ?? version} stage={{ w: 360, h: 500 }} onFront={() => { }} onClose={() => { void shell.close(); }}/>
        : view === 'quit' ? <QuitDialog open container={null} onCancel={() => { void shell.close(); }} onQuit={() => { void shell.quit().catch(fail); }}/>
                : view === 'panel' ? <div className="native-panel" ref={setWinEl}><SideContextMenu {...menu} standalone>{null}</SideContextMenu></div>
                    : <SideContextMenu {...menu}>
      <DesktopWindow ref={mergeRefs(setWinEl)} platform={platform} covered={onboarding && onboard !== 'leaving'} coverBar={onboarding && <OnboardingLanguage value={prefs.prefs.language} leaving={onboard === 'leaving'} onChange={v => prefs.set('language', v)}/>}
        cover={onboarding && <Onboarding key={onboardKey} platform={platform} pairing={pairing} perms={permissions} onGrant={grant} onOpenSettings={openPermissionSettings} preferences={prefs} container={winEl} leaving={onboard === 'leaving'} version={state.boot?.version ?? version} connected={devices.some(d => d.group === 'online')} onSkip={finishOnboarding} onFinish={finishOnboarding}/>}
        state={{ maximized, minimized: false, closed: false }} active={windowActive} title={tr('显示器排列')} onMinimize={() => { void windowAction('minimize'); }} onToggleMaximize={() => { void windowAction('toggleMaximize'); }} onClose={() => { void windowAction('close'); }} onActivate={() => { }} navOpen={navOpen} onToggleNav={() => { setNavOpen(!navOpen); if (navMode !== 'devices')
                        setNavMode('devices'); }} pages={{ add: navMode === 'add', settings: navMode === 'settings' }} onSideAction={action => { if (action === 'quit') {
                        if (prefs.prefs.confirmQuit)
                            setQuitAsk(true);
                        else
                            void shell.quit().catch(fail);
                    }
                    else {
                        setNavOpen(true);
                        setNavMode(navMode === action ? 'devices' : action);
                    } }} version={state.boot?.version ?? version} sideExtra={navMode === 'add' ? <SelfInfo container={winEl}/> : null} titleEnd={<>{prefs.prefs.upgradeHint && state.update?.candidate && state.update.phase !== 'disabled' && <UpgradeButton/>}<PromoArea sponsor={!prefs.presentation.hide_sponsor} container={winEl}/></>} titleCenter={self?.paused ? <span className="win__paused"><PauseGlyph/>{tr('共享已暂停')}</span> : null} navBack={platform === 'win' && navMode !== 'devices' ? { title: navMode === 'add' ? tr('添加设备') : tr('设置'), label: tr('返回设备列表'), onBack: () => setNavMode('devices') } : null} nav={collapsed => <PeekScope collapsed={collapsed} suspended={contextOpen} onTarget={id=>{setPeekId(id);if(!id)setGrowIds(previous=>previous.size?new Set():previous);}} render={id => { const requestId=fromRequestPeekId(id);if(requestId){const request=pairing.requests.find(r=>r.id===requestId);return request?<RequestRow key={request.id} req={request} collapsed={false} inPeek onExpand={()=>setNavOpen(true)} onAccept={()=>menu.onAccept(request)} onReject={()=>menu.onReject(request)}/>:null;} const d = devices.find(d => d.id === id); return d ? <DeviceItem key={id} inPeek volumeOpen={selectedDevice===id} d={d} selected={selectedDevice === id} collapsed={false} onSelect={() => selectDevice(id)} onToggle={(kind, mode) => toggle(id, kind, mode)} onPause={paused => { void invoke(id, 'pause', paused); }} onSpeaker={action => { void speaker(id, action); }} now={Date.now()} noExit={!hasExit(devices)}/> : null; }}>
          <div className="nav-stack"><AddHead open={navMode !== 'devices' && platform === 'mac'} title={navMode === 'add' ? tr('添加设备') : tr('设置')} onBack={() => setNavMode('devices')}/>
            <RequestsGroup pairing={pairing} collapsed={collapsed} onExpand={() => setNavOpen(true)} hidden={navMode === 'settings'}/>
            <div className="nav-view" key={navMode} data-view={navMode} data-anim={navSwitched ? '' : undefined}>{navMode === 'add' ? <AddPanel pairing={pairing}/> : navMode === 'settings' ? <SettingsNav platform={platform} query={query} onQuery={setQuery} active={section} onJump={id => setJump({ section: id, n: Date.now() })}/> : <DeviceList growIds={growIds} devices={devices} selectedId={selectedDevice} onSelect={selectDevice} onToggle={toggle} onPause={menu.onPause} onSpeaker={menu.onSpeaker} collapsed={collapsed} now={Date.now()}/>}</div>
          </div>
        </PeekScope>}>
        <div className="disp" data-hidden={navMode === 'settings' ? '' : undefined} aria-hidden={navMode === 'settings' || undefined} inert={navMode === 'settings'}>
          <VisuallyHidden id={hint}>{tr('方向键移动位置；Command 或 Ctrl 加方向键缩放；Shift 微调按键、拖动或缩放时按住可关闭吸附；Esc 取消选中')}</VisuallyHidden>
          <div className="disp__canvas" ref={canvas} aria-busy={savingLayout}>
            <div className="canvas-tools" role="toolbar" aria-label={tr('排列操作')}>
              {/* Two screens with arrows closing their gap: compaction after a manual edit. */}
              <Tooltip interactive={false} placement="bottom" content={tr('自动靠拢')}><Button variant="ghost" size="sm" iconOnly aria-label={tr('自动靠拢')} aria-pressed={compactLayout} onClick={() => prefs.savePresentation({ compact_layout: !compactLayout })}><Icon><path d="M2 5.5h6M6 3.5l2 2-2 2M18 5.5h-6M14 3.5l-2 2 2 2"/><rect x="2" y="10" width="7" height="5.5" rx="1.6"/><rect x="11" y="10" width="7" height="5.5" rx="1.6"/></Icon></Button></Tooltip>
              <span className="canvas-tools__sep"/>
              <Tooltip interactive={false} content={`${tr('撤销')}|${platform === 'mac' ? '⌘Z' : 'Ctrl+Z'}`} placement="bottom"><Button variant="ghost" size="sm" iconOnly aria-label={tr('撤销')} aria-keyshortcuts={platform === 'mac' ? 'Meta+Z' : 'Control+Z'} disabled={savingLayout || history.undoCount === 0} onClick={() => travel('undo')}><Icon><path d="M7.2 4.6 3.8 8l3.4 3.4M4.3 8h7.2a4.2 4.2 0 0 1 0 8.4H9.2"/></Icon></Button></Tooltip>
              <Tooltip interactive={false} content={`${tr('重做')}|${platform === 'mac' ? '⇧⌘Z' : 'Ctrl+Y'}`} placement="bottom"><Button variant="ghost" size="sm" iconOnly aria-label={tr('重做')} aria-keyshortcuts={platform === 'mac' ? 'Shift+Meta+Z' : 'Control+Y'} disabled={savingLayout || history.redoCount === 0} onClick={() => travel('redo')}><Icon><path d="M12.8 4.6 16.2 8l-3.4 3.4M15.7 8H8.5a4.2 4.2 0 0 0 0 8.4h2.3"/></Icon></Button></Tooltip>
              <span className="canvas-tools__sep"/><Tooltip interactive={false} placement="bottom" content={tr('重置排列')}><Button variant="ghost" size="sm" iconOnly aria-label={tr('重置排列')} disabled={savingLayout} onClick={() => { void resetLayout(); }}><Icon name="replay"/></Button></Tooltip>
            </div>
            <Arrangement platform={platform} screens={visible} geometryScreens={screens} onProjection={previewScreens} onScreensChange={(next, change) => { void saveScreens(mergeVisibleGroups(screens,next), change.label, change.merge); }} selectedId={selected} onSelect={selectScreen} settings={{ showHotZones: false, snapping: true, compact: compactLayout, padding: navOpen ? 24 : 48, step: 10 }} keyScope={canvas} hintId={hint} selectedGroup={!screenOnly && selectedDevice ? new Set(screens.filter(s => s.device === selectedDevice).map(s => s.id)) : undefined} contextFor={s => `device:${s.device}`}/>
          </div>
        </div>
        {settingsMounted && <SettingsPage open={navMode === 'settings'} platform={platform} api={prefs} capsIme={state.input?.mac_caps_switches_input_source ?? null} query={query} jump={jump} onActive={setSection} version={state.boot?.version ?? version} upgrade={{ available: false, next: '' }} perms={permissions} onGrant={grant} onOpenPermissionSettings={openPermissionSettings} onLog={() => { }} onAbout={menu.onAbout} onBlocklist={() => setBlockedOpen(true)} onOnboarding={replayOnboarding}/>}
      </DesktopWindow>
    </SideContextMenu>;
    // oink-ui ships built-in component strings (default aria labels, key names)
    // in zh-CN and en-US only; Japanese and Korean use its English strings.
    return <OinkProvider locale={language === 'zh-Hans' ? 'zh-CN' : 'en-US'} defaultAccent={DEFAULT_ACCENT.value}>
    <AppearanceSync local={prefs.local} presentation={prefs.presentation}/><div className="oink-app" data-view={view} data-platform={platform} data-edge-to-edge={maximized ? '' : undefined}>
      {view === 'main' && <div className="win-shadow" aria-hidden="true"><div className="win-shadow__shape"/></div>}
      {body}{view === 'main' && !maximized && <ResizeEdges platform={platform}/>}{(view==='main'||view==='panel')&&<StatusNotices/>}
      {(view==='main'||view==='panel') && state.error && <div role="alert" className="backend-notice"><Icon name="alert"/>{problemText(state.error)}<Button size="sm" variant="ghost" onClick={() => agent.fail('')}>{tr('关闭')}</Button></div>}
      {winEl && <>{!onboarding && <PairDialog pairing={pairing} container={winEl}/>}<BlockListDialog loading={state.blocked === null && !state.error} pending={unblocking} error={blockError ?? (!state.blocked && state.error ? problemText(state.error) : null)} open={blockedOpen} blocked={pairing.blocked} container={winEl} onClose={() => setBlockedOpen(false)} onRemove={p => { setBlockError(null); setUnblocking(ids => [...ids,p.id]); void pairing.actions.unblock(p).catch(() => setBlockError(problemText('unavailable'))).finally(() => setUnblocking(ids => ids.filter(id => id !== p.id))); }}/>
        {view === 'main' && <UpgradeDialog container={winEl}/>}
        <QuitDialog pendingOperations={state.pending.length} open={quitAsk} container={winEl} onCancel={() => setQuitAsk(false)} onQuit={() => { void shell.quit().catch(fail); }}/>
        <LeaveDialog device={devices.find(d => d.id === leaveId) ?? null} container={winEl} onCancel={() => setLeaveId(null)} onConfirm={() => { const d = devices.find(d => d.id === leaveId); if (d)
            void (d.group === 'self' ? api.leaveGroup() : api.removeMember(d.id)).then(() => setLeaveId(null)).catch(fail); }}/>
      </>}
      <AddressDialog device={address.device} addresses={address.addresses} container={winEl} pending={address.pending} problem={address.problem} onCancel={address.cancel} onConfirm={(d,ip,port)=>{void address.save(d,ip,port);}}/>
      <Confirmer />{winEl && <Toaster container={winEl} position="bottom-right" mobilePosition="bottom-right" max={3} />}
    </div>
  </OinkProvider>;
}
