import { useState } from 'react';
import { Input, Slider } from 'oink-ui';

const MIN = 0.1, MAX = 5;
/** Clamp a typed gain to the slider's range at the input's 0.01 precision. */
export const clampGain = (v: number) => Math.min(MAX, Math.max(MIN, Math.round(v * 100) / 100));

/**
 * A pointer gain: a coarse slider (0.05 steps) and a number field (0.01) that
 * show the same value. The slider reports while dragging and commits on
 * release; the field commits on blur or Enter.
 */
export function GainControl({ label, value, disabled, onChange, onCommit }: {
  label: string;
  value: number;
  disabled?: boolean;
  onChange: (v: number) => void;
  onCommit: (v: number) => void;
}) {
  const [draft, setDraft] = useState<string | null>(null);
  const commit = () => {
    if (draft === null) return;
    setDraft(null);
    const n = parseFloat(draft);
    if (!Number.isFinite(n)) return;
    const v = clampGain(n);
    if (v === value) return;
    onChange(v);
    onCommit(v);
  };
  return <span className="setp__gain">
    <Slider aria-label={label} min={MIN} max={MAX} step={0.05} digits={2} value={value} disabled={disabled}
      format={v => `${v.toFixed(2)}x`} onChange={onChange} onChangeEnd={onCommit}/>
    <Input size="sm" className="setp__gain-input" aria-label={label} inputMode="decimal" suffix="x" disabled={disabled}
      value={draft ?? value.toFixed(2)} onChange={v => setDraft(v.replace(/[^\d.]/g, ''))} onBlur={commit}
      onKeyDown={e => { if (e.key === 'Enter') commit(); else if (e.key === 'Escape') setDraft(null); }}/>
  </span>;
}
