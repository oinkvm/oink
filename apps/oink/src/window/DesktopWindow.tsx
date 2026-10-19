import { tr } from '../i18n';
import { inTauri } from '../api/transport';
import { forwardRef, useEffect, useRef, useState } from 'react';
import type { CSSProperties, ReactNode, TransitionEvent } from 'react';
import { Button, Icon, Tooltip, mergeRefs, presets, speed } from 'oink-ui';
import { useTrafficLightAlignment } from './useTrafficLightAlignment';
import { OverlayScrollbar } from './OverlayScrollbar';
export type Platform = 'mac' | 'win';
export interface WindowState {
    maximized: boolean;
    minimized: boolean;
    closed: boolean;
}
export interface StageSize {
    w: number;
    h: number;
}
export interface DesktopWindowProps {
    platform: Platform;
    state: WindowState;
    active: boolean;
    title: string;
    onMinimize: () => void;
    onToggleMaximize: () => void;
    onClose: () => void;
    onActivate: () => void;
    navOpen: boolean;
    onToggleNav: () => void;
    nav: (collapsed: boolean) => ReactNode;
    onSideAction?: (action: SideAction) => void;
    pages?: Partial<Record<SideAction, boolean>>;
    navBack?: {
        title: string;
        label: string;
        onBack: () => void;
    } | null;
    sideExtra?: ReactNode;
    version?: string;
    titleCenter?: ReactNode;
    titleEnd?: ReactNode;
    children: ReactNode;
    cover?: ReactNode;
    covered?: boolean;
    coverBar?: ReactNode;
}
export type SideAction = 'settings' | 'add' | 'quit';
const SIDE_ACTIONS: Array<{
    id: SideAction;
    label: string;
    icon: ReactNode;
}> = [
    { id: 'add', get label() {
            return tr('添加设备');
        }, icon: <Icon name="plus"/> },
    { id: 'settings', get label() {
            return tr('设置');
        }, icon: <Icon name="settings"/> },
    { id: 'quit', get label() {
            return tr('退出');
        }, icon: <Icon><path d="M10 3.2v6.3M6.1 5.6a6.2 6.2 0 1 0 7.8 0"/></Icon> },
];
function useNavCollapse(open: boolean) {
    const [collapsed, setCollapsed] = useState(!open);
    useEffect(() => {
        if (open) {
            setCollapsed(false);
            return;
        }
        const s = speed();
        if (s <= 0) {
            setCollapsed(true);
            return;
        }
        const t = window.setTimeout(() => setCollapsed(true), (presets.soft.duration + 300) * s);
        return () => window.clearTimeout(t);
    }, [open]);
    const onTransitionEnd = (e: TransitionEvent) => {
        if (!open && e.propertyName === 'width' && e.target === e.currentTarget)
            setCollapsed(true);
    };
    return { collapsed, onTransitionEnd };
}
export const DesktopWindow = forwardRef<HTMLDivElement, DesktopWindowProps>(function DesktopWindow({ platform, state, active, title, onMinimize, onToggleMaximize, onClose, onActivate, navOpen, onToggleNav, nav, onSideAction, pages = {}, navBack, sideExtra, version, titleCenter, titleEnd, cover, covered = false, coverBar, children }, ref) {
    const navState = useNavCollapse(navOpen);
    const [expanding, setExpanding] = useState(false);
    const wasOpen = useRef(navOpen);
    useEffect(() => {
        if (navOpen && !wasOpen.current) {
            setExpanding(true);
            const t = window.setTimeout(() => setExpanding(false), (presets.soft.duration + 100) * Math.max(speed(), 0));
            wasOpen.current = navOpen;
            return () => window.clearTimeout(t);
        }
        wasOpen.current = navOpen;
        setExpanding(false);
    }, [navOpen]);
    const sideScroll = useRef<HTMLDivElement>(null);
    useEffect(() => {
        if (!navOpen)
            sideScroll.current?.scrollTo({ top: 0, behavior: speed() > 0 ? 'smooth' : 'auto' });
    }, [navOpen]);
    const lastCenter = useRef<ReactNode>(null);
    if (titleCenter)
        lastCenter.current = titleCenter;
    const center = (<div className="win__title-center" role="status" data-shown={titleCenter ? '' : undefined} aria-hidden={!titleCenter || undefined}>
      {lastCenter.current}
    </div>);
    const navToggle = (<Button variant="ghost" size="sm" iconOnly className="win__nav-toggle" aria-label={navOpen ? tr('收起导航栏') : tr('展开导航栏')} aria-expanded={navOpen} aria-controls="win-nav" onClick={onToggleNav}>
      <Icon name={platform === 'mac' ? 'sidebar' : 'menu'}/>
    </Button>);
    const backBar = (b: NonNullable<DesktopWindowProps['navBack']>) => (<span className="win__nav-back" key="back">
      <Tooltip interactive={false} content={b.label} placement="bottom">
        <Button variant="ghost" size="sm" iconOnly className="win__nav-toggle" aria-label={b.label} onClick={b.onBack}>
          <Icon name="arrow-left"/>
        </Button>
      </Tooltip>
      <h2 className="win__nav-title">{b.title}</h2>
    </span>);
    const lastExtra = useRef<ReactNode>(null);
    if (sideExtra)
        lastExtra.current = sideExtra;
    const node = useRef<HTMLDivElement>(null);
    const style = { inset: 'var(--main-gutter, 0px)' } as CSSProperties;
    return (<div ref={mergeRefs(ref,node)} className="win" data-covered={covered ? '' : undefined} data-platform={platform} data-active={active?'':undefined} data-maximized={state.maximized?'':undefined} style={style} role="dialog" aria-label={title} onPointerDownCapture={onActivate} onFocusCapture={onActivate}>
      {platform === 'mac'
            ? <MacTitlebar state={state} onMinimize={onMinimize} onToggleMaximize={onToggleMaximize} onClose={onClose} extra={navToggle} center={center} end={titleEnd} coverBar={coverBar}/>
            : <WinTitlebar state={state} onMinimize={onMinimize} onToggleMaximize={onToggleMaximize} onClose={onClose} extra={navBack ? backBar(navBack) : <>{navToggle}<Lockup /></>} center={center} end={titleEnd} coverBar={coverBar}/>}
      <div className="win__body" inert={covered}>
        
        <aside id="win-nav" className="win__side" data-open={navOpen ? '' : undefined} data-expanding={expanding ? '' : undefined} onTransitionEnd={(e) => { navState.onTransitionEnd(e); if (navOpen && e.propertyName === 'width' && e.target === e.currentTarget)
        setExpanding(false); }}>
          <div className="win__side-scroll">
            
            <div className="win__side-clip" ref={sideScroll} onScroll={(e) => { if (e.currentTarget.scrollLeft)
        e.currentTarget.scrollLeft = 0; }}>
              <div className="win__side-inner" data-collapsed={navState.collapsed ? '' : undefined}>{nav(navState.collapsed)}</div>
            </div>
            
            <OverlayScrollbar target={sideScroll} disabled={!navOpen}/>
          </div>
          <div className="win__side-extra" data-open={sideExtra ? '' : undefined} aria-hidden={!sideExtra || undefined} ref={(el) => { if (el)
        el.inert = !sideExtra; }}>
            <div>{lastExtra.current}</div>
          </div>
          
          <div className="win__side-foot" style={{ '--n': SIDE_ACTIONS.length } as CSSProperties}>
            {SIDE_ACTIONS.map((a, i) => {
            const isPage = a.id in pages;
            const current = !!pages[a.id];
            const label = a.label;
            return (<span key={a.id} className="win__side-slot" style={{ '--i': i } as CSSProperties}>
                  
                  <Tooltip interactive={false} content={current ? tr('返回设备列表') : label} placement={navOpen ? 'top' : 'right'}>
                    <Button variant="ghost" size="sm" iconOnly aria-label={label} data-action={a.id} aria-pressed={isPage ? current : undefined} onClick={() => onSideAction?.(a.id)}>
                      {a.icon}
                    </Button>
                  </Tooltip>
                </span>);
        })}
            
            {version && <span className="win__side-ver" aria-label={tr('版本 {0}', version)}>v{version}</span>}
          </div>
        </aside>
        <div className="win__content">{children}</div>
      </div>
      {cover}
    </div>);
});
function Lockup() {
    return (<svg className="win__lockup" viewBox="0 0 226 80" role="img" aria-label="Oink">
      <g transform="translate(8 9.5)">
        <path className="brand-tail" d="M20.5 32C20.5 46 31 54.5 45 54.5A9.5 9.5 0 1 0 36.3 41.2" fill="none" strokeWidth="5.5" strokeLinecap="round"/>
        <path className="win__lockup-ink" d="M9 6V39L33.5 29.5Q23.5 15.5 9 6Z" strokeWidth="5" strokeLinejoin="round"/>
      </g>
      <g className="win__lockup-word" transform="translate(88 17) scale(0.72)" fill="none" strokeWidth="11" strokeLinecap="round" strokeLinejoin="round">
        <circle cx="32" cy="32" r="26.5"/>
        <path d="M79.5 25.5V58.5"/>
        <circle className="win__lockup-dot" cx="79.5" cy="7" r="6.8"/>
        <path d="M102.5 25.5V58.5M102.5 38.5a13 13 0 0 1 26 0V58.5"/>
        <path d="M151.5 5.5V58.5M151.5 44L172 25.5M159.5 37.5L174.5 58.5"/>
      </g>
    </svg>);
}
interface BarProps {
    state: WindowState;
    onMinimize: () => void;
    onToggleMaximize: () => void;
    onClose: () => void;
    extra?: ReactNode;
    center?: ReactNode;
    end?: ReactNode;
    coverBar?: ReactNode;
}
function MacTitlebar({ state, onMinimize, onToggleMaximize, onClose, extra, center, end, coverBar }: BarProps) {
    const bar = useRef<HTMLDivElement>(null);
    const offset = useTrafficLightAlignment(bar);
    return (<div ref={bar} className="win__titlebar" style={{'--native-controls-offset': `${offset}px`} as CSSProperties} data-tauri-drag-region onDoubleClick={(e) => { if (e.target === e.currentTarget)
        onToggleMaximize(); }}>
      <div className="mac-lights" style={inTauri ? {visibility:'hidden',pointerEvents:'none'} : undefined}>
        <button type="button" className="mac-light" data-kind="close" aria-label={tr('关闭')} onClick={onClose}>
          <svg viewBox="0 0 12 12" aria-hidden><path d="M3.5 3.5l5 5M8.5 3.5l-5 5"/></svg>
        </button>
        
        <button type="button" className="mac-light" data-kind="min" aria-label={tr('最小化')} disabled={state.maximized} onClick={onMinimize}>
          <svg viewBox="0 0 12 12" aria-hidden><path d="M2.8 6h6.4"/></svg>
        </button>
        <button type="button" className="mac-light" data-kind="zoom" aria-label={state.maximized ? tr('退出全屏') : tr('进入全屏')} onClick={onToggleMaximize}>
          <svg viewBox="0 0 12 12" aria-hidden>
            {state.maximized
            ? <path className="fill" d="M6.6 2.2v3.2h3.2zM5.4 9.8V6.6H2.2z"/>
            : <path className="fill" d="M3.2 3.2h4.2L3.2 7.4zM8.8 8.8H4.6l4.2-4.2z"/>}
          </svg>
        </button>
      </div>
      {extra}
      {center}
      {end != null && <div className="win__title-end">{end}</div>}
      {coverBar && <div className="win__cover-bar">{coverBar}</div>}
    </div>);
}
function WinTitlebar({ state, onMinimize, onToggleMaximize, onClose, extra, center, end, coverBar }: BarProps) {
    return (<div className="win__titlebar" data-tauri-drag-region onDoubleClick={(e) => { if (!(e.target as HTMLElement).closest('button'))
        onToggleMaximize(); }}>
      
      {extra}
      {center}
      {end != null && <div className="win__title-end">{end}</div>}
      {coverBar && <div className="win__cover-bar">{coverBar}</div>}
      <div className="win-caps">
        <button type="button" className="win-cap" aria-label={tr('最小化')} onClick={onMinimize}>
          <svg viewBox="0 0 10 10" aria-hidden><path d="M0 5.5h10"/></svg>
        </button>
        <button type="button" className="win-cap" data-window-maximize aria-label={state.maximized ? tr('向下还原') : tr('最大化')} onClick={onToggleMaximize}>
          <svg viewBox="0 0 10 10" aria-hidden>
            {state.maximized
            ? <path d="M2.5 2.5V1.5A1 1 0 0 1 3.5.5h5a1 1 0 0 1 1 1v5a1 1 0 0 1-1 1h-1M1.5 2.5h5a1 1 0 0 1 1 1v5a1 1 0 0 1-1 1h-5a1 1 0 0 1-1-1v-5a1 1 0 0 1 1-1z"/>
            : <rect x=".5" y=".5" width="9" height="9" rx="1"/>}
          </svg>
        </button>
        <button type="button" className="win-cap" data-kind="close" aria-label={tr('关闭')} onClick={onClose}>
          <svg viewBox="0 0 10 10" aria-hidden><path d="M.5.5l9 9M9.5.5l-9 9"/></svg>
        </button>
      </div>
    </div>);
}
export function AppIcon({ platform, size }: {
    platform: Platform;
    size: number;
}) {
    if (platform === 'win') {
        return (<svg className="app-icon app-icon--win" viewBox="8 8 240 240" width={size} height={size} aria-hidden>
        <rect x="8" y="8" width="240" height="240" rx="44" fill="#1B1E28"/>
        <g transform="translate(22.4 22.4) scale(3.3)">
          <g transform="translate(0 1.5)">
            <path d="M20.5 32C20.5 46 31 54.5 45 54.5A9.5 9.5 0 1 0 36.3 41.2" fill="none" className="brand-tail" strokeWidth="5.5" strokeLinecap="round"/>
            <path d="M9 6V39L33.5 29.5Q23.5 15.5 9 6Z" fill="#FFFFFF" stroke="#FFFFFF" strokeWidth="5" strokeLinejoin="round"/>
          </g>
        </g>
      </svg>);
    }
    return (<svg className="app-icon app-icon--mac" viewBox="100 100 824 824" width={size} height={size} aria-hidden>
      <rect x="100" y="100" width="824" height="824" rx="185" fill="#1B1E28"/>
      <g transform="translate(216 216) scale(9.25)">
        <g transform="translate(0 1.5)">
          <path d="M20.5 32C20.5 46 31 54.5 45 54.5A9.5 9.5 0 1 0 36.3 41.2" fill="none" className="brand-tail" strokeWidth="5.5" strokeLinecap="round"/>
          <path d="M9 6V39L33.5 29.5Q23.5 15.5 9 6Z" fill="#FFFFFF" stroke="#FFFFFF" strokeWidth="5" strokeLinejoin="round"/>
        </g>
      </g>
    </svg>);
}
