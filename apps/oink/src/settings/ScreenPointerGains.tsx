import { useState } from 'react';
import { Row } from 'oink-ui';
import { api } from '../api/client';
import { agent, useAgent } from '../api/store';
import type { Screen } from '../api/types';
import { tr } from '../i18n';
import { GainControl } from './GainControl';

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
    return <Row key={screen.key.display} label={screen.display.name}>
      <GainControl label={tr('{0}的远程指针速度', screen.display.name)} value={value} disabled={pending}
        onChange={gain => setDrafts(current => ({...current, [screen.key.display]: gain}))}
        onCommit={gain => { void save(screen, gain); }}/>
    </Row>;
  })}</div>;
}
