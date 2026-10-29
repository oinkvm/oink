import { tr } from '../i18n';
import type { CSSProperties } from 'react';
import { DropdownMenu, MenuLabel, MenuRadioGroup, MenuRadioItem, MenuSeparator, Row, Segmented, Switch } from 'oink-ui';
import type { Platform } from '../window/DesktopWindow';
import { KEY_NAMES, MAC_KEYS, WIN_KEYS, presetMap } from './prefs';
import type { KeyMap, KeyPreset, PrefsApi } from './prefs';
type Cap = {
    id?: string;
    u: number;
    ghost?: string;
};
const ROWS: Record<Platform, Cap[][]> = {
    mac: [
        [{ id: 'caps', u: 1.75 }, { u: 1, ghost: 'A' }, { u: 1, ghost: 'S' }, { u: 1, ghost: 'D' }],
        [{ id: 'shift', u: 2.25 }, { u: 1, ghost: 'Z' }, { u: 1, ghost: 'X' }],
        [{ id: 'fn', u: 1 }, { id: 'control', u: 1 }, { id: 'option', u: 1 }, { id: 'command', u: 1.25 }, { u: 3, ghost: '' }],
    ],
    win: [
        [{ id: 'caps', u: 1.75 }, { u: 1, ghost: 'A' }, { u: 1, ghost: 'S' }, { u: 1, ghost: 'D' }],
        [{ id: 'shift', u: 2.25 }, { u: 1, ghost: 'Z' }, { u: 1, ghost: 'X' }],
        [{ id: 'ctrl', u: 1.25 }, { id: 'win', u: 1 }, { id: 'alt', u: 1 }, { u: 3, ghost: '' }],
    ],
};
const targetName = (platform: Platform, t: string) => {
    if (t === 'none') return tr('mapping|无');
    const sided = { rightCtrl: tr('右 Ctrl'), rightAlt: tr('右 Alt'), rightShift: tr('右 Shift'), rightWin: tr('右 Win') };
    return sided[t as keyof typeof sided] ?? KEY_NAMES[platform === 'mac' ? 'win' : 'mac'][t]?.name ?? t;
};
export function KeyMapControl({ platform, api, capsIme, label, onLog }: {
    platform: Platform;
    api: PrefsApi;
    capsIme: boolean | null;
    label: string;
    onLog: (text: string) => void;
}) {
    const { prefs, setPrefs } = api;
    const preset = prefs.keyPreset;
    const custom = preset === 'custom';
    const split = platform === 'mac' && custom && prefs.capsSplit;
    const map: KeyMap = custom ? prefs.keyCustom[platform] : presetMap(platform, preset, capsIme === true);
    const targets = platform === 'mac' ? ['ctrl', 'win', 'alt', 'shift', 'caps'] : ['control', 'option', 'command', 'shift', 'caps'];
    const sources: string[] = platform === 'mac' ? [...MAC_KEYS, ...(custom && prefs.allowFnMapping ? ['fn'] : [])] : WIN_KEYS;
    const choosePreset = (v: KeyPreset) => {
        setPrefs((p) => {
            if (v === 'custom' && !p.keyCustomTouched[platform] && p.keyPreset !== 'custom') {
                const from = presetMap(platform, p.keyPreset, capsIme === true);
                return { ...p, keyPreset: v, keyCustom: { ...p.keyCustom, [platform]: from }, keyCustomTouched: { ...p.keyCustomTouched, [platform]: true }, capsSplit: platform === 'mac' ? !!from.capsLong : p.capsSplit };
            }
            return { ...p, keyPreset: v, keyCustomTouched: { ...p.keyCustomTouched, [platform]: p.keyCustomTouched[platform] || p.keyPreset === 'custom' } };
        });
        onLog(tr('按键映射 → {0}', { mac: tr('Mac 手感'), win: tr('Windows 手感'), custom: tr('自定义') }[v]));
    };
    const setKey = (src: string, t: string, long = false) => {
        setPrefs((p) => {
            const cur = p.keyCustom[platform];
            const next: KeyMap = long ? { ...cur, capsLong: t } : { ...cur, keys: { ...cur.keys, [src]: t } };
            return { ...p, keyCustom: { ...p.keyCustom, [platform]: next }, keyCustomTouched: { ...p.keyCustomTouched, [platform]: true } };
        });
        onLog(tr('自定义映射：{0}{1} → {2}', KEY_NAMES[platform][src].name, long ? tr('（长按）') : split && src === 'caps' ? tr('（短按）') : '', targetName(platform, t)));
    };
    const setSplit = (on: boolean) => {
        setPrefs((p) => {
            const cur = p.keyCustom[platform];
            const next = on && !cur.capsLong ? { ...cur, capsLong: cur.keys.caps } : cur;
            return { ...p, capsSplit: on, keyCustom: { ...p.keyCustom, [platform]: next } };
        });
        onLog(tr('Caps Lock 短按 / 长按分开映射：{0}', on ? tr('开') : tr('关')));
    };
    const mapped = (src: string) => {
        const short = targetName(platform, map.keys[src] ?? (src === 'fn' ? 'ctrl' : src));
        const long = src === 'caps' && (custom ? split : !!map.capsLong) && map.capsLong ? targetName(platform, map.capsLong) : null;
        return long ? `${short} / ${long}` : short;
    };
    const capFace = (src: string) => (<>
      <span className="kmap__to">{mapped(src)}</span>
      <span className="kmap__from">
        <span>{KEY_NAMES[platform][src].name}</span>
        {KEY_NAMES[platform][src].sym && <span className="kmap__sym">{KEY_NAMES[platform][src].sym}</span>}
      </span>
    </>);
    const keyMenu = (src: string) => {
        const name = KEY_NAMES[platform][src].name;
        const group = (label: string, value: string | undefined, onChange: (v: string) => void) => (<MenuRadioGroup aria-label={label} value={value ?? null} onChange={onChange}>
        
        {[...targets.filter(t => src !== 'fn' || t !== 'caps'), ...(src === 'fn' ? ['rightCtrl', 'rightAlt', 'rightShift', 'rightWin'] : []), 'none'].map((t) => <MenuRadioItem key={t} value={t} closeOnSelect={!(src === 'caps' && split)}>{targetName(platform, t)}</MenuRadioItem>)}
      </MenuRadioGroup>);
        return (<DropdownMenu key={src} matchTriggerWidth={false} trigger={<button type="button" className="kmap__key ok-focusable" data-key={src} aria-label={tr('{0}：映射为 {1}，点击修改', name, mapped(src))}>
            {capFace(src)}
          </button>}>
        {src === 'caps' && split ? (<>
            <MenuLabel>{tr('短按')}</MenuLabel>
            {group(tr('{0} 短按映射为', name), map.keys.caps, (v) => setKey('caps', v))}
            <MenuSeparator />
            <MenuLabel>{tr('长按')}</MenuLabel>
            {group(tr('{0} 长按映射为', name), map.capsLong, (v) => setKey('caps', v, true))}
          </>) : group(tr('{0} 映射为', name), map.keys[src] ?? (src === 'fn' ? 'ctrl' : undefined), (v) => setKey(src, v))}
      </DropdownMenu>);
    };
    const note = !custom
        ? platform === 'mac' && preset === 'mac' && !capsIme ? tr('Caps Lock 跟随系统设置：按原样发送 Caps Lock') : null
        : tr('点按键修改它对应的键');
    return (<div className="kmap">
      <Segmented<KeyPreset> size="sm" aria-label={tr('{0}：预设', label)} value={preset} onChange={choosePreset} options={[{ value: 'mac', get label() {
                return tr('Mac 手感');
            } }, { value: 'win', get label() {
                return tr('Windows 手感');
            } }, { value: 'custom', get label() {
                return tr('自定义');
            } }]}/>
      <div className="kmap__board" data-custom={custom ? '' : undefined} role="group" aria-label={tr('修饰键映射')}>
        {ROWS[platform].map((row, r) => (<div className="kmap__row" key={r}>
            {row.map((c, i) => {
                const style = { '--u': c.u } as CSSProperties;
                if (!c.id)
                    return <span key={i} className="kmap__key kmap__key--ghost" style={style} aria-hidden>{c.ghost}</span>;
                if (!sources.includes(c.id))
                    return c.id === 'fn' ? <span key={i} className="kmap__key kmap__key--ghost" style={style} aria-hidden>fn</span> : null;
                return custom
                    ? <span key={c.id} className="kmap__slot" style={style}>{keyMenu(c.id)}</span>
                    : <span key={c.id} className="kmap__key" style={style} data-key={c.id} role="img" aria-label={tr('{0} 映射为 {1}', KEY_NAMES[platform][c.id].name, mapped(c.id))}>{capFace(c.id)}</span>;
            })}
          </div>))}
      </div>
      {note && <p className="kmap__note">{note}</p>}
      {platform === 'mac' && custom && (<div className="kmap__options">
          <Row label={tr('Caps Lock 短按 / 长按分开映射')} description={tr('允许为短按和长按 Caps Lock 映射不同功能')}>
            <Switch aria-label={tr('Caps Lock 短按 / 长按分开映射')} checked={prefs.capsSplit} onChange={setSplit}/>
          </Row>
          <Row className="kmap__fn" label={tr('允许映射 Fn 键')} description={tr('Fn / 地球键的系统功能可能仍会在本机触发，映射的修饰键会同时发送到远端')}>
            <Switch aria-label={tr('允许映射 Fn 键')} checked={prefs.allowFnMapping} onChange={(on) => setPrefs(p => ({...p, allowFnMapping:on}))}/>
          </Row>
        </div>)}
    </div>);
}
