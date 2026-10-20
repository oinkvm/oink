import { useState } from 'react';
import { Row, Slider } from 'oink-ui';
import { api } from '../api/client';
import { agent, useAgent } from '../api/store';
import type { Screen } from '../api/types';
import { tr } from '../i18n';

export function ScreenPointerGains() {
  const { layout, overview } = useAgent();
  const [drafts, setDrafts] = useState<Record<string, number>>({});
  const [pending, setPending] = useState(false);
  const screens = layout?.geometry.screens.filter(screen => screen.key.device === overview?.this_device.id) ?? [];
  const save = async (screen: Screen, gain: number) => {
    setPending(true);
    try {
      await api.setScreenRemoteGain(screen.key, gain);
      await agent.query('layout', api.layout);
    } catch { agent.fail('unavailable'); }
    finally {
      setPending(false);
      setDrafts(current => { const next = {...current}; delete next[screen.key.display]; return next; });
    }
  };
  if (!screens.length) return <span className="setp__note">{tr('不可用')}</span>;
  return <div aria-busy={pending}>{screens.map(screen => {
    const value = drafts[screen.key.display] ?? screen.remote_gain ?? 1;
    const label = tr('{0}的远程指针速度', screen.display.name);
    return <Row key={screen.key.display} label={screen.display.name} controlWidth={240}>
      <span className="setp__slider">
        <Slider aria-label={label} min={0.1} max={5} step={0.1} digits={1} value={value} disabled={pending}
          format={v => `${v.toFixed(1)}x`}
          onChange={gain => setDrafts(current => ({...current, [screen.key.display]: gain}))}
          onChangeEnd={gain => { void save(screen, gain); }}/>
        <span className="setp__val" aria-hidden>{value.toFixed(1)}x</span>
      </span>
    </Row>;
  })}</div>;
}
