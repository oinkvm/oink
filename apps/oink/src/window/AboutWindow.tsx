import { useEffect, useRef, useState } from 'react';
import { Button, Icon, Tag } from 'oink-ui';
import { AppIcon } from './DesktopWindow';
import type { Platform, StageSize } from './DesktopWindow';
import { shell } from '../api/client';
import { inTauri } from '../api/transport';
import { tr } from '../i18n';
const REPO = 'https://github.com/oinkvm/oink';
interface LicenseComponent {
    id: string;
    name: string;
    version: string;
    build?: string | null;
    ecosystem: string;
    repository: string | null;
    licenseExpression: string;
    selectedLicense: string;
    copyright: string[];
    licenseFiles: {
        path: string;
        sha256: string;
    }[];
    scope: string[];
}
interface Inventory {
    schemaVersion: 1;
    components: LicenseComponent[];
}
const GITHUB = 'M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z';
function Wordmark() {
    return (<svg className="about__word" viewBox="-1 -1 183 67" role="img" aria-label="Oink">
      <g fill="none" stroke="currentColor" strokeWidth="11" strokeLinecap="round" strokeLinejoin="round">
        <circle cx="32" cy="32" r="26.5"/>
        <path d="M79.5 25.5V58.5"/>
        <circle cx="79.5" cy="7" r="6.8" fill="currentColor" stroke="none"/>
        <path d="M102.5 25.5V58.5M102.5 38.5a13 13 0 0 1 26 0V58.5"/>
        <path d="M151.5 5.5V58.5M151.5 44L172 25.5M159.5 37.5L174.5 58.5"/>
      </g>
    </svg>);
}
function Doodle() {
    return (<svg className="about__doodle" viewBox="0 0 360 160" aria-hidden>
      
      <path d="M24 151.5H336" strokeDasharray="1 7"/>
      
      <g>
        <circle className="about__fill" cx="61" cy="126" r="8"/>
        <path className="about__fill" d="M118 104C96 98 66 106 64 129C63 144 74 151 90 151H127C132 151 134 146 130 140C126 132 124 121 127 110Z"/>
        <path d="M80 151C80 137 93 129 106 137"/>
        <ellipse className="about__fill" cx="93" cy="150" rx="15" ry="4.5"/>
        <ellipse className="about__fill" cx="126" cy="150" rx="8" ry="3.8"/>
        
        <path className="about__fill" d="M120 76C108 80 102 94 104 106C105 113 110 115 114 111C118 106 118 92 124 80Z"/>
        <circle className="about__fill" cx="134" cy="90" r="22"/>
        
        <path className="about__fill" d="M122 72C115 77 112 88 113 100C114 110 118 116 123 115C129 114 130 104 131 94C132 85 133.5 78 134.5 73C135.5 68.5 131 66.5 126.5 67.5C124.5 68 123 69.8 122 72Z"/>
        <path d="M121 82C118 92 118 102 120 108" strokeWidth="1.6"/>
        <circle className="about__dot" cx="141" cy="86" r="2.4"/>
        <ellipse className="about__dot" cx="154.5" cy="94" rx="2" ry="1.5"/>
        <path d="M158 97l12-2.5M158 100.5l11 4"/>
        <path d="M142 100c2 3 6 3 8 0"/>
      </g>
      
      <path d="M180 62c-3-5-11-3-9 3 1 3 5 6 9 9 4-3 8-6 9-9 2-6-6-8-9-3z"/>
      
      <g>
        <path d="M321 113c8-2 11 5 6 8-5 2-7-5 0-9 4-2 8 0 9 3"/>
        <path d="M249 143v8M263 145v6M291 145v6M306 140v11"/>
        <path className="about__fill" d="M234 112C238 96 255 88 277 88C305 88 323 102 323 120C323 138 305 147 279 147C259 147 241 141 235 129Z"/>
        
        <path className="about__fill" d="M209 90C204 84 203 77 205.5 73C207.5 71 210.5 71.5 213 73C217.5 76 221 80 223 84Z"/>
        <path className="about__fill" d="M232 84C235.5 78 240 73.5 245 72C247.8 71.3 249.8 73 250 75.6C250.3 80 249 85.5 246 90Z"/>
        <path d="M209.5 77.5c1.6 1.4 3.2 3.4 4.4 5.6M245.6 77c-.7 2.2-1.6 4.4-2.8 6.4" strokeWidth="1.6"/>
        <circle className="about__fill" cx="226" cy="106" r="25"/>
        <ellipse className="about__fill" cx="205" cy="113" rx="11" ry="8.5"/>
        <circle className="about__dot" cx="201.5" cy="113" r="1.8"/>
        <circle className="about__dot" cx="208.5" cy="113" r="1.8"/>
        <circle className="about__dot" cx="219" cy="99" r="2.4"/>
        <path d="M232 117c3 2 7 2 9-1"/>
      </g>
    </svg>);
}
export function AboutWindow({ platform, open, front, version, build, stage, onFront, onClose }: {
    platform: Platform;
    open: boolean;
    front: boolean;
    version: string;
    build?: string | null;
    stage: StageSize;
    onFront: () => void;
    onClose: () => void;
}) {
  const close = (
    platform === 'mac' ? (
      <div className="mac-lights">
        <button type="button" className="mac-light" data-kind="close" aria-label={tr('关闭')} onClick={onClose}>
          <svg viewBox="0 0 12 12" aria-hidden><path d="M3.5 3.5l5 5M8.5 3.5l-5 5" /></svg>
        </button>
        <button type="button" className="mac-light" data-kind="min" aria-label={tr('最小化')} disabled><svg viewBox="0 0 12 12" aria-hidden /></button>
        <button type="button" className="mac-light" data-kind="zoom" aria-label={tr('全屏')} disabled><svg viewBox="0 0 12 12" aria-hidden /></button>
      </div>
    ) : (
      <div className="win-caps">
        <button type="button" className="win-cap" data-kind="close" aria-label={tr('关闭')} onClick={onClose}>
          <svg viewBox="0 0 10 10" aria-hidden><path d="M.5.5l9 9M9.5.5l-9 9" /></svg>
        </button>
      </div>
    )
  );

    const [page, setPage] = useState<'main' | 'licenses'>('main');
    const [inventory, setInventory] = useState<Inventory | null>(null);
    const [selected, setSelected] = useState<string | null>(null);
    const [terms, setTerms] = useState<string>('');
    const [error, setError] = useState(false);
    const termsGeneration = useRef(0);
    useEffect(() => {
        if (!open)
            return;
        void shell.attribution('third-party-licenses.json').then(text => {
            const parsed = JSON.parse(text) as Inventory;
            if (parsed.schemaVersion !== 1 || !Array.isArray(parsed.components))
                throw new Error('schema');
            setInventory(parsed);
        }).catch(() => setError(true));
    }, [open]);
    const showTerms = async (component: LicenseComponent) => {
        const generation = ++termsGeneration.current;
        setSelected(component.id);
        setTerms('');
        setError(false);
        try {
            const texts = await Promise.all(component.licenseFiles.map(file => shell.attribution(file.path)));
            if (termsGeneration.current === generation) setTerms([...component.copyright, ...texts].join('\n\n'));
        }
        catch {
            if (termsGeneration.current === generation) setError(true);
        }
    };
    void stage;
    return <div className="win about" data-platform={platform} data-active={front ? '' : undefined} data-front="" role="dialog" aria-label={tr('关于 Oink')} style={{ left: 0, top: 0, width: '100%', height: '100%' }} onPointerDownCapture={onFront} onKeyDown={e => { if (e.key === 'Escape')
        onClose(); }}>
    <div className="win__titlebar about__bar" data-tauri-drag-region>
      <span hidden={inTauri && platform === 'win'} style={inTauri && platform === 'mac' ? {visibility:'hidden',pointerEvents:'none'} : undefined}>{close}</span>
      {page === 'licenses' && <span className="about__crumb"><Button variant="ghost" size="sm" iconOnly aria-label={tr('返回')} onClick={() => { termsGeneration.current++; if (selected)
        setSelected(null);
    else
        setPage('main'); }}><Icon name="arrow-left"/></Button><span>{tr('开源软件引用')}</span></span>}
    </div>
    {page === 'main' && <Doodle />}
    {page === 'main' ? <div className="about__main">
      <AppIcon platform={platform} size={76}/><Wordmark /><p className="about__ver">{build==null?tr('版本 {0}', version):tr('版本 {0} ({1})', version, build)}</p>
      <Button className="about__repo" variant="ghost" size="sm" onClick={() => { void shell.openUrl(REPO); }} leading={<svg className="about__gh" viewBox="0 0 16 16" aria-hidden><path d={GITHUB}/></svg>}>github.com/oinkvm/oink</Button>
      <Button className="about__oss" variant="ghost" size="sm" trailing={<Icon name="chevron-right"/>} onClick={() => setPage('licenses')}>{tr('开源软件引用')}</Button>
      <Button variant="ghost" size="sm" onClick={() => { const generation=++termsGeneration.current; setPage('licenses'); setSelected('product'); setTerms(''); setError(false); void Promise.all(['LICENSE', 'LICENSE-ADDITIONAL-TERMS.md'].map(p => shell.attribution(p))).then(texts => {if(termsGeneration.current===generation)setTerms(texts.join('\n\n'));}).catch(() => {if(termsGeneration.current===generation)setError(true);}); }}>GPL-3.0 · {tr('附加条款')}</Button>
      <p className="about__copy">© 2026 Hachiwii</p>
    </div> : <div className="about__licenses ok-scroll">
      {error && <p role="alert">{tr('无法读取许可文件')}</p>}
      {selected ? <pre className="license-text">{terms || tr('正在读取…')}</pre> : <><p className="about__lead">{tr('Oink 使用了以下开源软件，感谢它们的作者。')}</p>
        {!inventory && <p role="status">{tr('正在读取…')}</p>}
        <ul>{inventory?.components.map(l => <li key={l.id}>
          <button type="button" className="about__license-link ok-focusable" onClick={() => { void showTerms(l); }}><span className="about__lic-name">{l.name}<span className="about__lic-ver">{l.version}</span></span><span className="about__lic-note">{l.copyright.join(' · ') || l.name} · {l.scope.join(', ')}</span></button>
          <Tag className="about__lic-tag" size="sm">{l.licenseExpression}</Tag>
          {l.repository && /^https?:\/\//.test(l.repository) && <Button size="sm" variant="ghost" iconOnly aria-label={tr('项目主页')} onClick={() => { void shell.openUrl(l.repository!); }}><Icon name="external"/></Button>}
        </li>)}</ul></>}
    </div>}
  </div>;
}
