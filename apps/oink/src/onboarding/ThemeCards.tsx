import { tr } from '../i18n';
import { useId } from 'react';
import type { ThemeMode } from 'oink-ui';

const TONES = {
  light: { desk: '#E3E4E9', win: '#FDFDFE', side: '#F4F4F6', line: '#DCDDE3', ink: '#B9BBC5', canvas: '#ECECEF', scr: '#D3D5DD', scr2: '#C4C7D1', edge: 'rgb(27 28 34 / .12)' },
  dark: { desk: '#0E0F12', win: '#1E1F24', side: '#26272D', line: '#383941', ink: '#5A5C67', canvas: '#16171B', scr: '#3A3C46', scr2: '#4B4E5A', edge: 'rgb(255 255 255 / .1)' },
} as const;
type Tone = keyof typeof TONES;

function MiniUI({ tone }: { tone: Tone }) {
  const c = TONES[tone];
  return (
    <g>
      <rect width="140" height="96" fill={c.desk} />
      <rect x="12" y="12" width="128" height="84" rx="9" fill={c.win} stroke={c.edge} />
      {}
      <path d="M21 12.5H50V95.5H21A8.5 8.5 0 0 1 12.5 87V21A8.5 8.5 0 0 1 21 12.5Z" fill={c.side} />
      <circle cx="20" cy="20" r="1.9" fill="#FF5F57" />
      <circle cx="26" cy="20" r="1.9" fill="#FEBC2E" />
      <circle cx="32" cy="20" r="1.9" fill="#28C840" />
      {[0, 1, 2].map((i) => (
        <g key={i} transform={`translate(0 ${32 + i * 13})`}>
          {i === 0 && <rect x="15" y="-5" width="32" height="10" rx="3" fill={c.line} opacity=".7" />}
          <circle cx="21" cy="0" r="3" fill={c.ink} />
          <rect x="27" y="-2.6" width={i === 1 ? 16 : 13} height="2.2" rx="1.1" fill={c.ink} />
          <rect x="27" y="1.2" width="9" height="1.8" rx=".9" fill={c.line} />
          {i === 1 && <circle cx="44" cy="0" r="1.6" className="ob-mini__accent" />}
        </g>
      ))}
      <rect x="16" y="84" width="7" height="5" rx="1.5" fill={c.line} />
      <rect x="26" y="84" width="7" height="5" rx="1.5" fill={c.line} />
      {}
      <rect x="54" y="18" width="80" height="72" rx="6" fill={c.canvas} />
      <rect x="64" y="34" width="34" height="21" rx="3.5" fill={c.scr} />
      <rect x="71" y="55" width="22" height="14" rx="3" fill={c.scr2} />
      <rect x="98" y="38" width="26" height="16" rx="3" className="ob-mini__sel" />
    </g>
  );
}

function Preview({ mode }: { mode: ThemeMode }) {
  const clip = useId();
  return (
    <svg className="ob-theme__art" viewBox="0 0 140 96" aria-hidden>
      {mode === 'system' ? (
        <>
          <defs><clipPath id={clip}><rect x="70" width="70" height="96" /></clipPath></defs>
          <MiniUI tone="light" />
          <g clipPath={`url(#${clip})`}><MiniUI tone="dark" /></g>
        </>
      ) : <MiniUI tone={mode} />}
    </svg>
  );
}

const MODES: Array<{ value: ThemeMode; label: string }> = [
  { value: 'system', get label() { return tr('跟随系统'); } },
  { value: 'light', get label() { return tr('浅色'); } },
  { value: 'dark', get label() { return tr('深色'); } },
];

export function ThemeCards({ value, onChange, labelledBy, disabled }: { disabled?: boolean; value: ThemeMode; onChange: (v: ThemeMode) => void; labelledBy?: string }) {
  const name = useId();
  return (
    <div className="ob-themes" role="radiogroup" aria-labelledby={labelledBy}>
      {MODES.map((m) => (
        <label key={m.value} className="ob-theme">
          <input
            type="radio" className="ob-theme__input" name={name} value={m.value}
            disabled={disabled} checked={value === m.value} onChange={() => onChange(m.value)}
          />
          <span className="ob-theme__card"><span className="ob-theme__clip"><Preview mode={m.value} /></span></span>
          <span className="ob-theme__label">{m.label}</span>
        </label>
      ))}
    </div>
  );
}
