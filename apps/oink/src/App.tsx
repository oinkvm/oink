import { LayoutProjectionPreview } from './arrange/projection';
import { useEffect, useId, useMemo, useRef, useState } from 'react';
import { Button, Confirmer, Icon, OinkProvider, Toaster, Tooltip, VisuallyHidden, mergeRefs, speed, toast } from 'oink-ui';
import { ApiFailure, api, shell } from './api/client';
import { agent, start, useAgent } from './api/store';
import { inTauri } from './api/transport';
import { presentControls, resourceAction } from './devices/project';
import { useGrant } from './settings/useGrant';
import { DeviceList } from './devices/DeviceList';
import { PauseGlyph } from './devices/icons';
import { SideContextMenu, type SideContextMenuProps } from './devices/DeviceMenu';
import type { DeviceControlFailure, DeviceControlReply } from './api/cards';
import { ControlActions, type ControlAction } from './devices/controlActions';
import { controlCommand, controlObservation, volumeFacts } from './devices/controlPlan';
import { canDisableInput, toggleShare } from './devices/data';
import type { ShareKind, ShareValue } from './devices/data';
import type { SpeakerAction, MicrophoneAction } from './devices/VolumeRow';
import { freeGroup, mergeVisibleGroups } from './arrange/groups';
import { Arrangement, type ScreenMove } from './arrange/Arrangement';
import type { DeviceKey, Rect } from './api/types';
import { bounds, type Screen } from './arrange/geometry';
import { useHistory } from './arrange/useHistory';
import { RENDER_SCALE, renderLayout, layoutPlacements, presentScreens, visibleScreens } from './arrange/adapter';
import { effectiveSystemSync, LayoutProfileFailure, type ArrangementSnapshot, type CommitRequest, type LayoutOperation, type OperationReply, type PreviewReply, type ProfileSummary } from './api/layoutProfiles';
import { nativeLayout, profileErrorText, profileName, sameArrangement } from './arrange/snapshot';
import { runOperation } from './arrange/operations';
import { compactScreens } from './arrange/compaction';
import { snapshotFromScreens } from './arrange/compactionEdit';
import { CompactIcon } from './arrange/CompactIcon';
import { nextSelection, type SelectMode } from './arrange/selection';
import { ScreenPreview } from './arrange/screenPreview';
import { CreateProfileDialog, ProfileList, ProfileTools } from './arrange/LayoutProfiles';
import { DesktopWindow } from './window/DesktopWindow';
import { ResizeEdges } from './window/ResizeEdges';
import { FileProviderDialog, useFileProviderExplainer } from './permissions/FileProviderDialog';
import { QuitDialog } from './window/QuitDialog';
import { AboutWindow } from './window/AboutWindow';
import { UpgradeButton, UpgradeDialog } from './window/Upgrade';
import { PackageDropArea, usePackageDrag } from './window/PackageDrop';
import { PromoArea } from './window/PromoArea';
import { StatusNotices } from './window/StatusNotices';
import { observePanel } from './window/panel';
import { revealWhenReady } from './window/ready';
import { Onboarding, OnboardingLanguage } from './onboarding/Onboarding';
import { LeaveDialog } from './devices/LeaveDialog';
import { AddressDialog } from './devices/AddressDialog';
import { useAddressEditor } from './devices/useAddressEditor';
import { AddHead, AddPanel, RequestsGroup, SelfInfo } from './pairing/AddPanel';
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
/** A history step's immutable operation endpoints. Rigid edits restore no native origins. */
interface StepMeta { before: ArrangementSnapshot; after: ArrangementSnapshot; native_restore_devices: DeviceKey[]; original_kind: 'load' | 'edit' | 'reset' }
const mergeMeta = (a: StepMeta | undefined, b: StepMeta | undefined): StepMeta | undefined => a && b
    ? { before: a.before, after: b.after, original_kind: a.original_kind, native_restore_devices: [...new Set([...a.native_restore_devices, ...b.native_restore_devices])] } : b ?? a;
const UNKNOWN_TOAST = 'layout-operation-unknown';
export function App() {
    const state = useAgent();
    const packageDrag = usePackageDrag();
    const language = useLanguage();
    const prefs = usePreferences();
    const view = new URLSearchParams(location.search).get('view') ?? 'main';
    const pairing = usePairing(view === 'main');
    const platform = state.boot?.platform === 'windows' ? 'win' : 'mac';
    const [controlTargets,setControlTargets]=useState<ReadonlyMap<string,ControlAction[]>>(()=>new Map());
    const [controls]=useState(()=>new ControlActions({
        read:(device,action)=>controlObservation(agent.get().cards,device,action),
        send:async(requestId,device,action)=>{
            const cards=agent.get().cards,card=cards?.cards.find(c=>c.device===device);
            const refused=(reason:DeviceControlFailure):DeviceControlReply=>({type:'device_control_completed',data:{requestId,result:{outcome:'refused',reason,unknownCode:null}}});
            if(!card)return refused('offline');
            if(action.kind==='card'&&action.resource==='screen'&&action.desired==='locked'){
                await api.lockFocusTo(device);
                return {type:'device_control_completed',data:{requestId,result:{outcome:'applied'}}};
            }
            if(action.kind==='speakerVolume'||action.kind==='microphoneVolume'){
                const facts=volumeFacts(cards,device,action.kind==='microphoneVolume');
                if(!facts)return refused('stale_revision'); // rebase if facts changed since observation
                if(action.kind==='speakerVolume'?facts.exit!==action.output:card.status?.microphone?.mode!=='source')return refused('route_changed');
                return facts.exit?api.setExitVolume(requestId,facts.token,action.volume,action.muted)
                    :api.setForwardingControls(requestId,facts.token,action.volume,action.muted,action.deferPersistence===true);
            }
            const command=controlCommand(card,action);
            if(command?.kind!=='invoke') {
                if(action.kind!=='microphone')return refused('unsupported');
                const reason=command?.kind==='unavailable'?command.reason:'state_unknown';
                return refused(reason==='state_unknown'||reason==='state_stale'?'state_conflict':reason==='remote_fault'?'resource_fault':reason);
            }
            return api.invokeCard(requestId,command.token);
        },
        refresh:()=>agent.query('cards',api.deviceCards),show:setControlTargets,fail:(reason,action)=>{
            if(action.kind==='microphone'||action.kind==='microphoneVolume') {
                toast.danger({title:tr('无法更改麦克风设置'),description:problemText(reason)});
            } else agent.fail(reason);
        },
    }));
    useEffect(()=>agent.onEvent(event=>{
        if(event.type==='emergency_stop'){controls.clear();return;}
        return event.type==='device_control_completed'&&controls.complete(event.data.requestId,event.data.result);
    }),[controls]);
    useEffect(()=>()=>controls.dispose(),[controls]);
    useEffect(()=>{controls.snapshot();},[state.cards,controls]);
    const devices=presentControls(state.cards,controlTargets,state.permissions);
    // Native/menu callbacks may run again before React renders. Always build the
    // next toggle from the newest intent, never a captured render's old value.
    const currentDevices=()=>presentControls(agent.get().cards,controls.targets(),agent.get().permissions);
    const self = devices.find(d => d.group === 'self');
    const [navOpen, setNavOpen] = useState(true);
    const [maximized,setMaximized] = useState(false);
    const [windowActive,setWindowActive] = useState(true);
    const [navMode, setNavModeState] = useState<'devices' | 'add' | 'settings'>('devices');
    const [navSwitched,setNavSwitched] = useState(false);
    const [settingsMounted,setSettingsMounted] = useState(false);
    const [screenOnly,setScreenOnly] = useState(false);
    const [query, setQuery] = useState('');
    const [section, setSection] = useState<SectionId>('general');
    const [jump, setJump] = useState<SettingsJump | null>(null);
    const [selectedDevice, setSelectedDevice] = useState<string | null>(null);
    const [selected, setSelected] = useState<string | null>(null);
    // The canvas selection (whole computers, or one system-arranged screen); see selection.ts.
    const [selection, setSelection] = useState<string[]>([]);
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
    const [winEl, setWinEl] = useState<HTMLDivElement | null>(null);
    const canvas = useRef<HTMLDivElement>(null);
    const hint = useId();
    // History holds complete arrangement snapshots (placements, visibility, native
    // origins, active profile). Every edit, Load and replay is a backend operation.
    const history = useHistory<ArrangementSnapshot | null, StepMeta>(() => null, { equals: sameArrangement, mergeMeta });
    const profiles = state.layoutProfiles;
    const [operating, setOperating] = useState(false);
    const operatingRef = useRef(false);
    const savingLayout = operating || profiles?.operation?.status === 'running' || profiles?.operation?.status === 'outcome_unknown';
    const [createOpen, setCreateOpen] = useState(false);
    const [screenPreview, setScreenPreview] = useState<Record<string, Rect> | null>(null);
    const geometry = state.layout ? nativeLayout(state.layout, history.value?.native, history.value) : null;
    const screens = presentScreens(geometry ? renderLayout(geometry,state.wallpapers,history.value?.placements) : [], devices);
    // A device-local editing preference; only manual canvas edits compact.
    const compactLayout = prefs.presentation.compact_layout;
    const setNavMode = (mode: typeof navMode) => {
        if (mode === navMode) return;
        if (mode === 'settings') setSettingsMounted(true);
        setNavSwitched(true); setNavModeState(mode);
        winEl?.querySelector('.win__side-clip')?.scrollTo({top:0});
    };
    const clearSelection = () => { setSelectedDevice(null); setSelected(null); setSelection([]); };
    const selectDevice = (id: string) => {
        if (navOpen && selectedDevice === id) { setSelectedDevice(null); setSelected(null); setSelection([]); return; }
        if (devices.find(d => d.id === id)?.group === 'offline') return;
        setSelectedDevice(id); setScreenOnly(false);
        setSelected(screens.find(s => s.device === id)?.id ?? null);
        setSelection(screens.filter(s => s.device === id).map(s => s.id));
    };
    const selectScreen = (id: string | null, mode?: Exclude<SelectMode, 'plain'>) => {
        // Only a multi-screen computer writing its system arrangement has single screens.
        const single = (screen: string) => {
            const member = visible.find(s => s.id === screen), device = member?.device;
            return !!member && !!device && independent(member) && editable.has(device) && screens.filter(s => s.device === device).length > 1;
        };
        const next = id ? nextSelection(selection, visible, id, mode ?? 'plain', single) : [];
        const primary = id && next.includes(id) ? id : next[0] ?? null;
        setSelected(primary); setSelection(next); setScreenOnly(true);
        const screen = screens.find(s => s.id === primary);
        const device = devices.find(d => d.id === screen?.device);
        setSelectedDevice(device && device.group !== 'offline' ? device.id : null);
    };
    const selectedGone = !!selectedDevice && !devices.some(d => d.id === selectedDevice && d.group !== 'offline');
    useEffect(() => { if (selectedGone) { setSelectedDevice(null); setSelected(null); setSelection([]); } }, [selectedGone]);

    useEffect(() => { void start(view); }, []);
    useEffect(() => view !== 'panel' && inTauri ? revealWhenReady(() => { void shell.ready().catch(() => agent.fail('unavailable')); }) : undefined, [view]);
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

    // Authoritative state replaces the present value, except while an operation runs:
    // its own echoes must not overwrite the checkpoint it may still restore.
    useEffect(() => {
        if (!operatingRef.current && profiles?.snapshot) history.sync(profiles.snapshot);
    }, [profiles, operating]);
    useEffect(() => {
        if (view !== 'panel' || !winEl || !self || !prefs.local) return;
        return observePanel(winEl);
    }, [view,winEl,self?.id,!!prefs.local]);
    useEffect(() => {
        if (state.boot?.platform === 'windows' && prefs.local) void shell.setStatusIcon(prefs.local.show_status_icon).catch(() => agent.fail('unavailable'));
    }, [state.boot?.platform,prefs.local?.show_status_icon]);
    useEffect(() => {if(prefs.local)void shell.setLogLevel(prefs.local.log_level).catch(()=>agent.fail('unavailable'));},[prefs.local?.log_level]);
    useEffect(() => {if(prefs.local)void shell.setLanguage(prefs.local.ui_language).catch(()=>agent.fail('unavailable'));},[prefs.local?.ui_language]);
    useEffect(() => {
        const quality = view==='main'&&navMode==='settings'&&(section==='diagnostics'||query.trim().length>0);
        void shell.liveViews({stats:false,card_signals:view==='main'||view==='panel',group_quality:quality,audio_quality:quality,diagnostics:false}).catch(()=>agent.fail('unavailable'));
    },[view,navMode,section,query]);
    /** @deprecated Legacy bottom-centred notice. New code must use the in-app oink-ui toast instead. */
    const fail = (e: unknown) => agent.fail(e instanceof ApiFailure ? e.problem.code : 'unavailable');
    const permissionPage = () => { setNavMode('settings'); setNavOpen(true); setJump({ section: 'permissions', n: Date.now() }); };
    const invoke = async (id: string, resource: string, desired: string | boolean | number) => {
        const card = agent.get().cards?.cards.find(c => c.device === id);
        if (resource === 'speaker') {
            controls.set(id,{kind:'speaker',mode:desired as 'unused'|'forwarding'|'exit'});return;
        }
        if (resource === 'screen' && desired === 'locked') {
            controls.set(id,{kind:'card',resource:'screen',desired});return;
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
        if (resource === 'screen' && (desired === 'shared' || desired === 'disabled')
            && !(desired === 'shared' && (currentDevices().find(d=>d.id===id)?.cursor==='locked' || card.status?.cursor.lockedDevice===id))) {
            const present = history.value;
            if (!present || savingLayout) return;
            const patch = { device: id, shared: desired === 'shared' };
            const screen_shared = [...(present.screen_shared ?? []).filter(d => d.device !== id), patch];
            await commitEdit({ ...present, screen_shared }, tr('屏幕'), undefined,
                { placements: present.placements, native_plan_tokens: [], disabled: [], screen_shared: [patch] }, []);
            return;
        }
        controls.set(id,{kind:'card',resource:resource as Extract<ControlAction,{kind:'card'}>['resource'],desired});
    };
    const changeShare = (id: string, kind: ShareKind, desired: ShareValue) => { void invoke(id, kind, desired); };
    const toggle = (id: string, kind: ShareKind, click: 'single' | 'double' = 'single') => {
        const d = currentDevices().find(d => d.id === id);
        if (!d) return;
        if (kind === 'speaker') {
            controls.set(id, {kind:'speaker', mode:click==='double'?(d.speaker.mode==='exit'?'unused':'exit'):d.speaker.mode==='unused'?'forwarding':'unused'});
            return;
        }
        if (click === 'double' && (kind === 'keyboard' || kind === 'mouse')) {
            if (canDisableInput(d, kind)) void invoke(id, kind, 'disabled');
            return;
        }
        const next = toggleShare(d, kind);
        if (!next) { permissionPage(); return; }
        void invoke(id, kind, next[kind] as ShareValue);
    };
    const speaker = (id: string, action: SpeakerAction) => {
        const d=currentDevices().find(d=>d.id===id);
        if(!d)return;
        controls.set(id,{kind:'speakerVolume',output:d.speaker.mode==='exit',volume:action.type==='mute'?d.speaker.volume:action.value,
            muted:action.type==='mute'?!d.speaker.muted:false,final:action.type==='mute'||action.final===true,
            deferPersistence:action.type==='volume'&&action.dragging===true&&d.speaker.mode!=='exit'});
    };
    const microphone = (id:string,action:MicrophoneAction)=>{
        const d=currentDevices().find(d=>d.id===id);
        if(!d)return;
        const install=()=>{setNavMode('settings');setNavOpen(true);setJump({section:'audio',item:'virtualMicrophone',n:Date.now()});};
        if(action==='install') {if(d.group==='self')install();return;}
        if(action==='receive' && !d.microphone?.virtualReady || action==='shared' && !d.microphone?.permissionGranted) {
            const receive=action==='receive';
            // Settings only manage this device, so a remote fix names the device instead of offering a button.
            if(d.group!=='self') {
                toast.warning({title:receive?tr('请在 {0} 上安装虚拟麦克风',d.name):tr('请在 {0} 上授予麦克风权限',d.name)});
                return;
            }
            toast.warning({title:receive?tr('未安装虚拟麦克风'):tr('未获取麦克风权限'),
                action:{label:receive?tr('安装'):tr('授权'),onClick:receive?install:()=>grantDevicePermission(id,'microphone')}});
            return;
        }
        if(action==='shared'||action==='local'||action==='receive') {
            controls.set(id,{kind:'microphone',mode:action==='shared'?'source':action==='local'?'muted':'receiver'});return;
        }
        if(!d.microphone)return;
        controls.set(id,{kind:'microphoneVolume',volume:action.type==='mute'?d.microphone.volume:action.value,
            muted:action.type==='mute'?!d.microphone.muted:false,final:action.type==='mute'||action.final===true});
    };
    const grantDevicePermission=(id:string,kind:'input_monitoring'|'microphone')=>{
        if(!state.cards?.cards.some(c=>c.device===id&&c.isSelf))return;
        void api.requestPermission(kind).then(()=>agent.query('permissions',api.permissions)).catch(error=>{
            if(kind==='microphone')toast.danger({title:tr('无法更改麦克风设置'),description:problemText(error instanceof ApiFailure?error.problem.code:'backend_failed')});
            else fail(error);
        });
    };
    const deviceName = (device: DeviceKey) => devices.find(d => d.id === device)?.name ?? device.slice(0, 8);
    /** Owner refusals of an operation, one line per computer. */
    const ownerProblems = (op: LayoutOperation) => {
        const lines = op.owners.filter(o => o.error).map(o => tr('{0}：{1}', deviceName(o.device), profileErrorText(o.error!)));
        return lines.length ? lines.join('\n') : op.error ? profileErrorText(op.error) : undefined;
    };
    const failureText = (e: unknown) => e instanceof LayoutProfileFailure ? profileErrorText(e.problem) : problemText(e instanceof ApiFailure ? e.problem.code : 'unavailable');
    /** One accepted operation at a time; its echoes stay buffered until it ends. */
    const operate = async (start: (requestId: string) => Promise<OperationReply>) => {
        operatingRef.current = true; setOperating(true);
        try {
            return await runOperation(start, () => toast.info({ id: UNKNOWN_TOAST, title: tr('正在确认排列结果'), description: tr('与设备的连接中断了，确认后会更新'), duration: 0 }));
        } finally { toast.dismiss(UNKNOWN_TOAST); operatingRef.current = false; setOperating(false); }
    };
    const token = () => agent.get().layoutProfiles?.token ?? '';
    const isBusy = () => operatingRef.current || ['running', 'outcome_unknown'].includes(agent.get().layoutProfiles?.operation?.status ?? '');
    /** An edit is recorded at once and settled to the owners' observed result. */
    const commitEdit = async (after: ArrangementSnapshot, label: string, merge: string | undefined, request: Omit<CommitRequest, 'request_id' | 'expected_token'>, nativeDevices: DeviceKey[]) => {
        const checkpoint = history.checkpoint(), before = checkpoint.present;
        if (!before || isBusy()) return;
        history.commit(after, label, merge, { before, after, native_restore_devices: nativeDevices, original_kind: 'edit' });
        try {
            const op = await operate(request_id => api.commitArrangement({ request_id, expected_token: token(), ...request }));
            if (op.status !== 'failed' && op.after && op.history_required) {
                const observed = op.after;
                history.settle(observed, m => m && { ...m, after: observed, native_restore_devices: [...new Set([...m.native_restore_devices, ...op.native_restore_devices])] });
            }
            else history.restore(checkpoint);
            if (op.status === 'partial') toast.warning({ title: tr('排列只应用到了部分设备'), description: ownerProblems(op) });
            else if (op.status === 'failed') toast.danger({ title: tr('无法应用排列'), description: ownerProblems(op) });
        }
        catch (e) {
            history.restore(checkpoint);
            toast.danger({ title: tr('无法应用排列'), description: failureText(e) });
        }
    };
    const [projection] = useState(() => new LayoutProjectionPreview(api.previewLayoutProjection));
    useEffect(() => () => projection.close(), [projection]);
    const previewScreens = (next: Screen[] | null, held = false) => {
        if (previewer.active || (projection.lingering && !held)) return;
        if (!state.layout || !next) {
            projection.end();
            return;
        }
        projection.update(layoutPlacements(state.layout, mergeVisibleGroups(screens, next)));
    };
    const independent = (s: Screen) => !!profiles?.devices.some(d => d.device === s.device && effectiveSystemSync(d.sync_system_arrangement, d.availability !== 'offline' && !s.offline));
    /** Compaction changes only placements. A native drag reuses its final normal
     * preview token, so the drag and whole-computer translation commit together. */
    const saveSnapshot = (after: ArrangementSnapshot, label: string, merge?: string, preview?: PreviewReply) => {
        const present = history.value;
        if (!present || savingLayout || sameArrangement(present, after)) return;
        const previous = preview && present.native.find(n => n.device === preview.native.device);
        const nativeChanged = preview && previous && preview.native.screens.some(s => {
            const old = previous.screens.find(o => o.screen.display === s.screen.display);
            return !old || old.origin.x !== s.origin.x || old.origin.y !== s.origin.y;
        });
        return commitEdit(after, label, merge, { placements: after.placements,
            native_plan_tokens: nativeChanged ? [preview.plan_token] : [], disabled: [] }, nativeChanged ? [preview.native.device] : []);
    };
    const saveScreens = (next: Screen[], label: string, merge?: string) => {
        const present = history.value;
        if (!present || savingLayout) return;
        return saveSnapshot(snapshotFromScreens(present, screens, next), label, merge);
    };
    const travel = async (direction: 'undo' | 'redo') => {
        if (savingLayout || !history.value) return;
        const checkpoint = history.checkpoint();
        const move = history[direction]();
        const meta = move?.meta;
        if (!move) return;
        if (!meta) { history.restore(checkpoint); return; }
        // Replay the entry's immutable endpoints, never a source rebuilt from echoes.
        const [from, target] = direction === 'undo' ? [meta.after, meta.before] : [meta.before, meta.after];
        try {
            // `original_kind` tells a switch (selection replays too) from a manual change
            // (the current profile stays and receives the replayed delta).
            const op = await operate(request_id => api.replayArrangement({ request_id, expected_token: token(), from, target, native_restore_devices: meta.native_restore_devices, direction, original_kind: meta.original_kind }));
            // The history transition already happened; the observed state only
            // replaces the present value, keeping the redo stack.
            if (op.status !== 'failed' && op.after) history.sync(op.after);
            else history.restore(checkpoint);
            if (op.status === 'partial') toast.warning({ title: tr('排列只恢复了部分设备'), description: ownerProblems(op) });
            else if (op.status === 'failed') toast.danger({ title: direction === 'undo' ? tr('无法撤销') : tr('无法重做'), description: ownerProblems(op) });
        }
        catch (e) {
            history.restore(checkpoint);
            toast.danger({ title: direction === 'undo' ? tr('无法撤销') : tr('无法重做'), description: failureText(e) });
        }
    };
    // A profile switch is one history entry with the operation's own endpoints (History contract).
    const loadProfile = async (profile: ProfileSummary) => {
        const current = agent.get().layoutProfiles;
        if (savingLayout || !current || canvas.current?.querySelector('[data-interacting]')) return;
        const checkpoint = history.checkpoint(), name = profileName(profile.name);
        try {
            const op = await operate(request_id => api.loadLayoutProfile({ request_id, expected_token: current.token, profile_id: profile.id }));
            if (op.status !== 'failed' && op.after) {
                if (op.history_required) {
                    history.sync(op.before);
                    history.commit(op.after, tr('切换到布局「{0}」', name), undefined, { before: op.before, after: op.after, native_restore_devices: op.native_restore_devices, original_kind: 'load' });
                } else history.sync(op.after);
            } else history.restore(checkpoint);
            if (op.status === 'partial') toast.warning({ title: tr('「{0}」只应用到了部分设备', name), description: ownerProblems(op) });
            else if (op.status === 'failed') toast.danger({ title: tr('无法切换到「{0}」', name), description: ownerProblems(op) });
        }
        catch (e) {
            history.restore(checkpoint);
            toast.danger({ title: tr('无法切换到「{0}」', name), description: failureText(e) });
        }
    };
    /** Create snapshots the live layout into a new current profile; not a canvas history
     * step. A refusal (including stale_state) keeps the dialog and its name for resubmission. */
    const createProfile = async (name: string) => {
        const current = agent.get().layoutProfiles;
        if (!current) return problemText('unavailable');
        try {
            agent.adoptProfiles(await api.createLayoutProfile({ request_id: crypto.randomUUID(), expected_token: current.token, name }));
            return null;
        } catch (e) { return failureText(e); }
    };
    const manageProfiles = () => { setNavMode('settings'); setNavOpen(true); setJump({ section: 'layout', item: 'manageLayouts', n: Date.now() }); };
    const renameProfile = async (profile: ProfileSummary, name: string) => {
        const current = agent.get().layoutProfiles;
        if (!current) return problemText('unavailable');
        try {
            agent.adoptProfiles(await api.renameLayoutProfile({ request_id: crypto.randomUUID(), expected_catalog_token: current.catalog_token, profile_id: profile.id, name }));
            return null;
        } catch (e) { return failureText(e); }
    };
    const deleteProfile = async (profile: ProfileSummary) => {
        const current = agent.get().layoutProfiles;
        if (!current) return;
        try {
            agent.adoptProfiles(await api.deleteLayoutProfile({ request_id: crypto.randomUUID(), expected_catalog_token: current.catalog_token, profile_id: profile.id }));
        } catch (e) { toast.danger({ title: tr('无法删除「{0}」', profileName(profile.name)), description: failureText(e) }); }
    };
    // One native screen dragged inside a system-arranged computer: coalesced backend
    // previews while moving, one commit with the final plan token on release.
    const editable = new Set((profiles?.devices ?? []).filter(d => effectiveSystemSync(d.sync_system_arrangement, d.availability !== 'offline') && d.can_edit_screens).map(d => d.device));
    const previewRects = (reply: PreviewReply) => {
        const placement = reply.placements.find(p => p.device === reply.native.device);
        if (!placement) return {};
        return Object.fromEntries(reply.native.screens.map(s => [`${s.screen.device}:${s.screen.display}`, {
            x: (placement.origin.x + s.origin.x * placement.scale) * RENDER_SCALE, y: (placement.origin.y + s.origin.y * placement.scale) * RENDER_SCALE,
            w: s.size.w * placement.scale * RENDER_SCALE, h: s.size.h * placement.scale * RENDER_SCALE,
        }]));
    };
    const [previewer] = useState(() => new ScreenPreview(api.previewSystemArrangement, token, reply => {
        setScreenPreview(previewRects(reply));
        const current = agent.get().layoutProfiles?.snapshot;
        if (current) projection.update(reply.placements, { ...current, placements: reply.placements,
            native: current.native.map(n => n.device === reply.native.device ? reply.native : n) });
    }));
    const moveScreen = async (move: ScreenMove) => {
        const screen = screens.find(s => s.id === move.id), key = state.layout?.geometry.screens.find(s => `${s.key.device}:${s.key.display}` === move.id)?.key;
        if (!screen || !key) return;
        if (move.type === 'drag') {
            if (savingLayout && !previewer.active) return;
            if (!previewer.active) previewer.begin(key);
            previewer.update({ x: move.desired.x / RENDER_SCALE, y: move.desired.y / RENDER_SCALE });
            return;
        }
        if (move.cancelled || !previewer.active) { previewer.cancel(); projection.close(); setScreenPreview(null); return; }
        let reply: PreviewReply;
        try { reply = await previewer.finish(); }
        catch (e) {
            previewer.cancel(); projection.close(); setScreenPreview(null);
            if (e instanceof LayoutProfileFailure || e instanceof ApiFailure) toast.danger({ title: tr('无法移动屏幕'), description: failureText(e) });
            return;
        }
        previewer.end();
        projection.linger();
        const present = history.checkpoint().present;
        if (!present) { setScreenPreview(null); return; }
        const placements = present.placements.map(p => reply.placements.find(r => r.device === p.device && r.device === reply.native.device) ?? p);
        const native = present.native.map(n => n.device === reply.native.device ? reply.native : n);
        const released = { ...present, placements, native };
        const draftGeometry = geometry && nativeLayout(geometry, native, released);
        if (!draftGeometry) { setScreenPreview(null); return; }
        const draft = presentScreens(renderLayout(draftGeometry, state.wallpapers, placements), devices);
        const visibleIds = new Set(visible.map(s => s.id));
        // A native preview constrains this computer's internal geometry. Resolve
        // canvas collisions on release just like a rigid drag, before compaction:
        // move this computer (including hidden siblings), keeping every peer fixed.
        const shown = draft.filter(s => visibleIds.has(s.id));
        const group = shown.filter(s => s.device === screen.device), from = bounds(group);
        const free = freeGroup(group, from, from, shown.filter(s => s.device !== screen.device));
        const dx = free.x - from.x, dy = free.y - from.y;
        const releasedScreens = draft.map(s => s.device === screen.device && (dx !== 0 || dy !== 0)
            ? { ...s, x: s.x + dx, y: s.y + dy } : s);
        const next = move.compact ? compactScreens(releasedScreens, releasedScreens.filter(s => visibleIds.has(s.id)), new Set([move.id])) : releasedScreens;
        const after = snapshotFromScreens(released, draft, next);
        // Stage the complete native result, including the original gesture and
        // compaction, then commit it through the same manual-history boundary.
        projection.update(after.placements, after);
        projection.linger();
        const done = saveSnapshot(after, move.label, undefined, reply);
        setScreenPreview(null);
        await done;
    };
    useEffect(() => {
        const key = (event: KeyboardEvent) => {
            if (onboarding || event.defaultPrevented || event.isComposing || navMode === 'settings' || pairing.dialog.open || quitAsk || blockedOpen || createOpen || leaveId || address.device || canvas.current?.querySelector('[data-interacting]') || !(event.metaKey || event.ctrlKey) || event.altKey || (event.target instanceof HTMLElement && event.target.closest('input,textarea,[contenteditable=true]'))) return;
            if (event.key.toLowerCase() === 'z') { event.preventDefault(); void travel(event.shiftKey ? 'redo' : 'undo'); }
            else if (event.ctrlKey && event.key.toLowerCase() === 'y') { event.preventDefault(); void travel('redo'); }
        };
        document.addEventListener('keydown', key); return () => document.removeEventListener('keydown', key);
    });
    // One-shot refreshes on navigation/activation; ongoing observations are backend events.
    useEffect(() => {
        if (view !== 'main') return;
        const refresh = () => {
            if (document.visibilityState === 'hidden') return;
            void agent.query('permissions', api.permissions).catch(() => {});
            void agent.query('fileProvider', api.fileProviderStatus).catch(() => {});
        };
        refresh();
        window.addEventListener('focus', refresh);
        document.addEventListener('visibilitychange', refresh);
        return () => { window.removeEventListener('focus', refresh); document.removeEventListener('visibilitychange', refresh); };
    }, [view, navMode, windowActive]);
    // Enabling the File Provider is explained first; only OK opens Finder.
    const fileProvider = useFileProviderExplainer();
    const enableFileProvider = async () => { if (await fileProvider.ask()) await api.openFileProviderEnable(); };
    useEffect(() => agent.onEvent(event => {
        if (event.type !== 'file_provider_required') return;
        toast.warning({ id: 'file-provider-required', title: tr('需要启用文件提供服务'),
            action: { label: tr('启用'), onClick: () => { void enableFileProvider().catch(error =>
                toast.danger({ title: tr('启用文件提供服务'), description: problemText(error instanceof ApiFailure ? error.problem.code : 'unavailable') })); } } });
        return true;
    }));
    const grant = async (id: PermissionId) => {
        if (id === 'files')
            await enableFileProvider();
        else if (id === 'audio')
            await api.requestPermission('audio_capture');
        else
            await api.requestPermission(id === 'input' ? 'input_monitoring' : id === 'network' ? 'local_network' : id);
    };
    const openPermissionSettings = async (id: PermissionId) => {
        if (id === 'files') await enableFileProvider();
        else await api.openPermissionSettings(id === 'input' ? 'input_monitoring' : id === 'network' ? 'local_network' : id === 'audio' ? 'audio_capture' : 'accessibility');
    };
    const accessibilityGrant = useGrant(grant, openPermissionSettings);
    // A screen is hidden by its own bit or by its device's screen sharing being off.
    const menuScreens = (geometry?.geometry.screens ?? []).map(s => {
        const sharing = history.value?.screen_shared?.find(d => d.device === s.key.device);
        const own = history.value?.disabled.find(d => d.screen.device === s.key.device && d.screen.display === s.key.display)?.disabled ?? s.hidden;
        return { id: `${s.key.device}:${s.key.display}`, key: s.key, device: s.key.device, name: s.display.name, hidden: own || (sharing ? !sharing.shared : s.device_disabled) };
    });
    const hiddenScreens = new Map<string, number>();
    for (const s of menuScreens) if (s.hidden) hiddenScreens.set(s.device, (hiddenScreens.get(s.device) ?? 0) + 1);
    const menu: Omit<SideContextMenuProps, 'children'> = {
        devices, screens: menuScreens,
        screenEditPending: savingLayout,
        onHideScreen: id => {
            const present = history.value;
            const key = menuScreens.find(s => s.id === id)?.key;
            if (!present || !key || savingLayout) return;
            const patch = { screen: key, disabled: true };
            const disabled = [...present.disabled.filter(d => d.screen.device !== key.device || d.screen.display !== key.display), patch];
            void commitEdit({ ...present, disabled }, tr('隐藏此屏幕'), undefined,
                { placements: present.placements, native_plan_tokens: [], disabled: [patch] }, []);
        },
        // Hiding all turns the device's screen sharing off; showing all turns it back on and
        // clears every per-screen hidden bit of the device, as one undoable edit.
        onScreensHidden: (id, hidden) => {
            if (hidden) { changeShare(id, 'screen', 'disabled'); return; }
            const present = history.value;
            if (!present || savingLayout) return;
            const patches = present.disabled.filter(d => d.screen.device === id && d.disabled).map(d => ({ screen: d.screen, disabled: false }));
            const sharedNow = present.screen_shared?.find(d => d.device === id)?.shared;
            const off = sharedNow === false || (sharedNow === undefined && devices.find(d => d.id === id)?.screen === 'disabled');
            if (!patches.length && !off) return;
            const sharing = off ? [{ device: id, shared: true }] : [];
            const disabled = present.disabled.map(d => d.screen.device === id ? { ...d, disabled: false } : d);
            const screen_shared = off ? [...(present.screen_shared ?? []).filter(d => d.device !== id), ...sharing] : present.screen_shared;
            void commitEdit({ ...present, disabled, ...(screen_shared ? { screen_shared } : {}) }, tr('显示全部屏幕'), undefined,
                { placements: present.placements, native_plan_tokens: [], disabled: patches, ...(off ? { screen_shared: sharing } : {}) }, []);
        },
        requests: pairing.requests, nearby: pairing.nearby, onShare: changeShare,
        onGameMode: (id, mode) => { void invoke(id, 'game', mode); }, onSpeaker: speaker, onMicrophone: microphone, onGrantPermission: grantDevicePermission,
        onPause: (id, paused) => { void invoke(id, 'pause', paused); }, onLeave: setLeaveId,
        onEditAddress: id => { const device=devices.find(d=>d.id===id); if(device)void address.open(device); },
        onShowWindow: () => { void shell.open('main').catch(fail); }, onAbout: () => { void shell.open('about').catch(fail); },
        onQuit: () => { void shell.quit().catch(fail); },
        onAccept: req => { if(view==='panel')void shell.open('main').then(()=>pairing.actions.accept(req)).catch(fail);else pairing.actions.accept(req); }, onReject: req => { void pairing.actions.reject(req).catch(fail); },
        onPair: p => { void pairing.actions.connect(p).catch(fail); }, onBlock: p => { void pairing.actions.block(p).catch(fail); },
    };
    const visible = useMemo(()=>visibleScreens(screens, geometry, devices.map(d => {
        const sharing = history.value?.screen_shared?.find(s => s.device === d.id);
        return sharing ? { ...d, screenSharing: sharing.shared, screen: sharing.shared ? 'shared' : 'disabled' } : d;
    })),[screens,geometry,devices,history.value]);
    // A device-local preference; geometry changes only after a manual gesture.
    const toggleCompact = () => {
        if (savingLayout) return;
        const on = !compactLayout;
        prefs.savePresentation({ compact_layout: on });
    };
    useEffect(()=>{if(selected&&!visible.some(s=>s.id===selected)){setSelected(null);setSelection([]);}},[visible,selected]);
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
    const localStatus = state.cards?.cards.find(card => card.section === 'this_device')?.status;
    if ([localStatus?.mouse.fault?.code, localStatus?.keyboard.fault?.code].includes('capture_unavailable')) {
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
      <DesktopWindow ref={mergeRefs(setWinEl)} platform={platform} covered={packageDrag || onboarding && onboard !== 'leaving'} coverBar={packageDrag ? <span className="package-drop-title">Oink</span> : onboarding && <OnboardingLanguage value={prefs.prefs.language} leaving={onboard === 'leaving'} onChange={v => prefs.set('language', v)}/>}
        cover={packageDrag ? <PackageDropArea platform={platform}/> : onboarding && <Onboarding key={onboardKey} platform={platform} pairing={pairing} perms={permissions} onGrant={grant} onOpenSettings={openPermissionSettings} preferences={prefs} container={winEl} leaving={onboard === 'leaving'} version={state.boot?.version ?? version} connected={devices.some(d => d.group === 'online')} onSkip={finishOnboarding} onFinish={finishOnboarding}/>}
        state={{ maximized, minimized: false, closed: false }} active={windowActive} title={tr('显示器排列')} onMinimize={() => { void windowAction('minimize'); }} onToggleMaximize={() => { void windowAction('toggleMaximize'); }} onClose={() => { void windowAction('close'); }} onActivate={() => { }} navOpen={navOpen} onSideBlank={clearSelection} onToggleNav={() => { if (navOpen) clearSelection(); setNavOpen(!navOpen); if (navMode !== 'devices')
                        setNavMode('devices'); }} pages={{ add: navMode === 'add', settings: navMode === 'settings' }} onSideAction={action => { if (action === 'quit') {
                        if (prefs.prefs.confirmQuit)
                            setQuitAsk(true);
                        else
                            void shell.quit().catch(fail);
                    }
                    else {
                        setNavOpen(true);
                        setNavMode(navMode === action ? 'devices' : action);
                    } }} version={state.boot?.version ?? version} sideExtra={navMode === 'add' ? <SelfInfo container={winEl}/> : null} titleEnd={<><UpgradeButton/><PromoArea sponsor={!prefs.presentation.hide_sponsor} container={winEl}/></>} titleCenter={self?.paused ? <span className="win__paused"><PauseGlyph/>{tr('共享已暂停')}</span> : null} navBack={platform === 'win' && navMode !== 'devices' ? { title: navMode === 'add' ? tr('添加设备') : tr('设置'), label: tr('返回设备列表'), onBack: () => setNavMode('devices') } : null} nav={collapsed => <>
          <div className="nav-stack">
            {platform === 'win' && <RequestsGroup pairing={pairing} collapsed={collapsed} onExpand={() => setNavOpen(true)} hidden={navMode === 'settings'}/>}
            <div className="nav-view" key={navMode} data-view={navMode} data-anim={navSwitched ? '' : undefined}>{platform === 'mac' && <><AddHead open={navMode !== 'devices'} title={navMode === 'add' ? tr('添加设备') : tr('设置')} onBack={() => setNavMode('devices')}/><RequestsGroup pairing={pairing} collapsed={collapsed} onExpand={() => setNavOpen(true)} hidden={navMode === 'settings'}/></>}{navMode === 'add' ? <AddPanel pairing={pairing}/> : navMode === 'settings' ? <SettingsNav platform={platform} query={query} onQuery={setQuery} active={section} onJump={id => setJump({ section: id, n: Date.now() })}/> : <DeviceList onGrantPermission={grantDevicePermission} onGrantAccessibility={() => { void accessibilityGrant.grant('accessibility'); }} devices={devices} selectedId={selectedDevice} onSelect={id => { setNavOpen(true); selectDevice(id); }} onToggle={toggle} onPause={menu.onPause} onSpeaker={menu.onSpeaker} onMicrophone={microphone} hiddenScreens={hiddenScreens} collapsed={collapsed} now={Date.now()}/>}</div>
          </div>
        </>}>
        <div className="disp" data-hidden={navMode === 'settings' ? '' : undefined} aria-hidden={navMode === 'settings' || undefined} inert={navMode === 'settings'}>
          <VisuallyHidden id={hint}>{tr('方向键移动位置；Command 或 Ctrl 加方向键缩放；Shift 微调按键、拖动或缩放时按住可关闭吸附；Esc 取消选中')}</VisuallyHidden>
          <div className="disp__canvas" ref={canvas} aria-busy={savingLayout}>
            <div className="canvas-tools" role="toolbar" aria-label={tr('排列操作')}>
              <ProfileTools state={profiles} busy={savingLayout} onLoad={p => { void loadProfile(p); }} onCreate={() => setCreateOpen(true)} onManage={manageProfiles}/>
              <span className="canvas-tools__sep"/>

              <Tooltip interactive={false} content={`${tr('撤销')}|${platform === 'mac' ? '⌘Z' : 'Ctrl+Z'}`} placement="bottom"><Button variant="ghost" size="sm" iconOnly aria-label={tr('撤销')} aria-keyshortcuts={platform === 'mac' ? 'Meta+Z' : 'Control+Z'} disabled={savingLayout || history.undoCount === 0} onClick={() => { void travel('undo'); }}><Icon><path d="M7.2 4.6 3.8 8l3.4 3.4M4.3 8h7.2a4.2 4.2 0 0 1 0 8.4H9.2"/></Icon></Button></Tooltip>
              <Tooltip interactive={false} content={`${tr('重做')}|${platform === 'mac' ? '⇧⌘Z' : 'Ctrl+Y'}`} placement="bottom"><Button variant="ghost" size="sm" iconOnly aria-label={tr('重做')} aria-keyshortcuts={platform === 'mac' ? 'Shift+Meta+Z' : 'Control+Y'} disabled={savingLayout || history.redoCount === 0} onClick={() => { void travel('redo'); }}><Icon><path d="M12.8 4.6 16.2 8l-3.4 3.4M15.7 8H8.5a4.2 4.2 0 0 0 0 8.4h2.3"/></Icon></Button></Tooltip>
              <span className="canvas-tools__sep"/>
              {/* Two screens with arrows closing their gap: compaction after a manual edit. */}
              <Tooltip interactive={false} placement="bottom" content={tr('自动靠拢')}><Button variant="ghost" size="sm" iconOnly aria-label={tr('自动靠拢')} aria-pressed={compactLayout} disabled={savingLayout} onClick={toggleCompact}><CompactIcon/></Button></Tooltip>
            </div>
            <Arrangement platform={platform} screens={visible} geometryScreens={screens} onProjection={previewScreens} onScreensChange={(next, change) => { void saveScreens(next, change.label, change.merge); }} selectedId={selected} onSelect={selectScreen} settings={{ showHotZones: false, snapping: true, compact: compactLayout, padding: navOpen ? 24 : 48, step: 10 }} keyScope={canvas} hintId={hint} selectedGroup={!screenOnly && selectedDevice ? new Set(screens.filter(s => s.device === selectedDevice).map(s => s.id)) : selection.length > 1 ? new Set(selection) : undefined} contextFor={s => `screen:${s.id}`} screenEditable={s => !!s.device && editable.has(s.device) && independent(s)} screenPreview={screenPreview} onScreenMove={move => { void moveScreen(move); }}/>
          </div>
        </div>
        {settingsMounted && <SettingsPage open={navMode === 'settings'} platform={platform} api={prefs} capsIme={state.input?.mac_caps_switches_input_source ?? null} query={query} jump={jump} onActive={setSection} version={state.boot?.version ?? version} upgrade={{ available: false, next: '' }} perms={permissions} onGrant={grant} onOpenPermissionSettings={openPermissionSettings} onLog={() => { }} onAbout={menu.onAbout} onBlocklist={() => setBlockedOpen(true)} layoutProfiles={<ProfileList state={profiles} onRename={renameProfile} onDelete={deleteProfile}/>} onOnboarding={replayOnboarding}/>}
      </DesktopWindow>
    </SideContextMenu>;
    // oink-ui ships built-in component strings (default aria labels, key names)
    // in zh-CN and en-US only; Japanese and Korean use its English strings.
    return <OinkProvider locale={language === 'zh-Hans' ? 'zh-CN' : 'en-US'} defaultAccent={DEFAULT_ACCENT.value}>
    <AppearanceSync local={prefs.local} presentation={prefs.presentation}/><div className="oink-app" data-package-drag={view==='main'&&packageDrag?'':undefined} data-view={view} data-platform={platform} data-edge-to-edge={maximized ? '' : undefined}>
      {view === 'main' && <div className="win-shadow" aria-hidden="true"><div className="win-shadow__shape"/></div>}
      {body}{view === 'main' && !maximized && <ResizeEdges platform={platform}/>}{(view==='main'||view==='panel')&&!packageDrag&&<StatusNotices/>}
      {winEl && <>{!onboarding && <PairDialog pairing={pairing} container={winEl}/>}<BlockListDialog loading={state.blocked === null} pending={unblocking} error={blockError} open={blockedOpen} blocked={pairing.blocked} container={winEl} onClose={() => setBlockedOpen(false)} onRemove={p => { setBlockError(null); setUnblocking(ids => [...ids,p.id]); void pairing.actions.unblock(p).catch(() => setBlockError(problemText('unavailable'))).finally(() => setUnblocking(ids => ids.filter(id => id !== p.id))); }}/>
        {view === 'main' && <UpgradeDialog container={winEl}/>}
        {view === 'main' && <CreateProfileDialog open={createOpen} state={profiles} container={winEl} onClose={() => setCreateOpen(false)} onSubmit={createProfile}/>}
        <FileProviderDialog {...fileProvider.dialog} container={winEl}/>
        <QuitDialog pendingOperations={[...controlTargets.values()].reduce((count,actions)=>count+actions.length,0)} open={quitAsk} container={winEl} onCancel={() => setQuitAsk(false)} onQuit={() => { void shell.quit().catch(fail); }}/>
        <LeaveDialog device={devices.find(d => d.id === leaveId) ?? null} container={winEl} onCancel={() => setLeaveId(null)} onConfirm={() => { const d = devices.find(d => d.id === leaveId); if (d)
            void (d.group === 'self' ? api.leaveGroup() : api.removeMember(d.id)).then(() => setLeaveId(null)).catch(fail); }}/>
      </>}
      <AddressDialog device={address.device} addresses={address.addresses} container={winEl} pending={address.pending} problem={address.problem} onCancel={address.cancel} onConfirm={(d,ip,port)=>{void address.save(d,ip,port);}}/>
      <Confirmer />{winEl && <Toaster container={winEl} position="bottom-right" mobilePosition="bottom-right" max={3} />}
    </div>
  </OinkProvider>;
}
