import { useRef, useState } from 'react';
import type { Layout } from '../api/types';
import { Arrangement } from './Arrangement';
import { layoutPlacements, renderLayout } from './adapter';
import { mergeVisibleGroups } from './groups';
import { useHistory } from './useHistory';

// A native desktop fixture with negative origins, a step, a portrait screen,
// and a single-screen computer. It uses the production adapter and group history.
function fixture(rearranged = false): Layout {
  const rows = [
    {
      device: 'local',
      key: 'main',
      name: 'This computer',
      x: 0,
      y: 0,
      w: 1600,
      h: 1000,
      offset: 0,
    },
    {
      device: 'local',
      key: 'side',
      name: 'This computer',
      x: rearranged ? 0 : -1000,
      y: rearranged ? -700 : 200,
      w: 1000,
      h: 700,
      offset: 0,
    },
    {
      device: 'remote',
      key: 'main',
      name: 'Another computer',
      x: 0,
      y: 0,
      w: 1400,
      h: 900,
      offset: 2000,
    },
    {
      device: 'remote',
      key: 'portrait',
      name: 'Another computer',
      x: 1400,
      y: -300,
      w: 700,
      h: 1200,
      offset: 2000,
    },
    {
      device: 'remote',
      key: 'top',
      name: 'Another computer',
      x: 0,
      y: -800,
      w: 1200,
      h: 800,
      offset: 2000,
    },
    {
      device: 'single',
      key: 'main',
      name: 'Single-screen computer',
      x: 0,
      y: 0,
      w: 1400,
      h: 900,
      offset: 4500,
    },
  ];
  return {
    revision: 1,
    source: 'manual',
    geometry: {
      bounds: { x: -1000 / 4096, y: -800 / 4096, w: 6900 / 4096, h: 1800 / 4096 },
      screens: rows.map(row => ({
        key: { device: row.device, display: row.key },
        device_name: row.name,
        os: 'macos',
        display: {
          key: row.key,
          name: row.key,
          bounds: { x: row.x, y: row.y, w: row.w, h: row.h },
          pixel_width: row.w,
          pixel_height: row.h,
          reference_extent: null,
          rotation_deg: 0,
          is_primary: row.key === 'main',
          is_builtin: false,
          is_virtual: false,
        },
        origin: { x: (row.offset + row.x) / 4096, y: row.y / 4096 },
        size: { w: row.w / 4096, h: row.h / 4096 },
        size_source: 'reported',
        gain: 1,
        hidden: false,
        device_disabled: false,
        online: true,
      })),
    },
  };
}

export function GroupPreview() {
  const [rearranged, setRearranged] = useState(false);
  const layout = fixture(rearranged);
  const history = useHistory(() => layoutPlacements(layout, []));
  const [selected, setSelected] = useState<string | null>(null);
  const [hidden, setHidden] = useState(false);
  const [compact, setCompact] = useState(false);
  const scope = useRef<HTMLDivElement>(null);
  const screens = renderLayout(layout, {}, history.value);
  return (
    <div
      ref={scope}
      style={{ position: 'fixed', inset: 0, background: 'var(--ok-well)', color: 'var(--ok-text)' }}
    >
      <div
        style={{
          position: 'absolute',
          top: 12,
          left: 16,
          right: 16,
          zIndex: 8,
          display: 'flex',
          gap: 12,
          alignItems: 'center',
        }}
      >
        <strong>Rigid computer groups</strong>
        <button onClick={() => history.undo()} disabled={!history.undoCount}>Undo</button>
        <button onClick={() => history.redo()} disabled={!history.redoCount}>Redo</button>
        <button onClick={() => {
          history.commit(layoutPlacements(layout, []), 'Reset');
          setSelected(null);
        }}>
          Reset
        </button>
        <button onClick={() => setRearranged(value => !value)}>Rearrange OS screens</button>
        <label>
          <input
            type="checkbox"
            checked={hidden}
            onChange={event => setHidden(event.target.checked)}
          />
          Hide one screen
        </label>
        <label>
          <input
            type="checkbox"
            checked={compact}
            onChange={event => setCompact(event.target.checked)}
          />
          Compact on release
        </label>
      </div>
      <Arrangement
        screens={screens.filter(screen => !hidden || screen.id !== 'local:side')}
        geometryScreens={screens}
        selectedId={selected}
        onSelect={setSelected}
        onScreensChange={(next, change) => history.commit(
          layoutPlacements(layout, mergeVisibleGroups(screens, next)), change.label, change.merge,
        )}
        keyScope={scope}
        settings={{ showHotZones: false, snapping: true, compact, padding: 48, step: 10 }}
      />
    </div>
  );
}
