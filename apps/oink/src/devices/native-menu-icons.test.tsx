// @vitest-environment jsdom
// Export the real device-menu SVGs; Rust embeds these reviewed snapshots without
// a Node dependency. OINK_UPDATE_MENU_ICONS=1 npm test -w apps/oink -- native-menu-icons
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeAll, expect, it } from 'vitest';
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { OinkProvider } from 'oink-ui';
import { SideContextMenu } from './DeviceMenu';
import { Glyph } from './icons';
import { setLanguage } from '../i18n';
import type { Device } from './types';

const require = createRequire(import.meta.url);
const libraryCss = readFileSync(require.resolve('oink-ui/styles.css'), 'utf8');
const here = dirname(fileURLToPath(import.meta.url));
const deviceCss = readFileSync(join(here, 'devices.css'), 'utf8');
const directory = join(here, '../../src-tauri/icons/menu');
const noop = () => {};
const deviceFixture: Device = {
  id: 'self', name: 'Synthetic', os: 'win', group: 'self', ip: '', port: 0, latency: 0, lastSeen: null,
  cursor: 'local', screen: 'shared', screenSharing: true, mouse: 'shared', keyboard: 'shared', clipboard: 'shared', canDisableMouse: true,
  speaker: { mode: 'unused', volume: 50, volumeKnown: true, muted: false, canMakeExit: true }, gameMode: 'auto',
};
beforeAll(() => {
  Object.defineProperty(window, 'matchMedia', { value: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) });
});
afterEach(cleanup);
function snapshot(name: string, svg: SVGElement) {
  const copy = svg.cloneNode(true) as SVGElement;
  const rule = svg.classList.contains('ok-icon')
    ? libraryCss.match(/\.ok-icon\{([^}]+)\}/)![1]
    : deviceCss.match(/\.devmenu__glyph svg\s*\{([^}]+)\}/)![1];
  const viewBox = copy.getAttribute('viewBox')!;
  for (const attr of [...copy.attributes]) copy.removeAttribute(attr.name);
  copy.setAttribute('xmlns', 'http://www.w3.org/2000/svg');
  copy.setAttribute('viewBox', viewBox);
  for (const declaration of rule.split(';')) {
    const [key, value] = declaration.split(':').map(s => s.trim());
    if (['fill', 'stroke', 'stroke-width', 'stroke-linecap', 'stroke-linejoin'].includes(key)) copy.setAttribute(key, value);
  }
  for (const filled of copy.querySelectorAll('.fill')) {
    filled.setAttribute('fill', 'currentColor');
    filled.setAttribute('stroke', 'none');
    filled.removeAttribute('class');
  }
  const result = '<!-- Generated from DeviceMenu/Glyph and their CSS; see native-menu-icons.test.tsx. -->\n' + copy.outerHTML + '\n';
  const file = join(directory, `${name}.svg`);
  if (process.env.OINK_UPDATE_MENU_ICONS === '1') {
    mkdirSync(directory, { recursive: true });
    writeFileSync(file, result);
  }
  expect(readFileSync(file, 'utf8'), name).toBe(result);
}
async function open(device: Device) {
  setLanguage('en');
  render(<OinkProvider defaultMotion="reduced"><SideContextMenu devices={[device]} requests={[]} nearby={[]}
    onShare={noop} onGameMode={noop} onPause={noop} onLeave={noop} onEditAddress={noop}
    onShowWindow={noop} onAbout={noop} onQuit={noop} onSpeaker={noop} onHideScreen={noop} onScreensHidden={noop}
    onAccept={noop} onReject={noop} onPair={noop} onBlock={noop}>
    <button data-ctx="tray:self">Target</button>
  </SideContextMenu></OinkProvider>);
  fireEvent.contextMenu(screen.getByRole('button', { name: 'Target' }));
  await screen.findByRole('menu');
}
it('keeps native menu SVGs identical to the current in-app menu, including stateful icons', async () => {
  await open(deviceFixture);
  for (const [name, label] of [
    ['lock', 'Lock pointer'], ['hide-screens', 'Hide all screens'], ['keyboard', 'Input devices'],
    ['game', 'Game mode'], ['audio', 'Set as audio output'], ['pause', 'Pause sharing'],
    ['show', 'Show window'], ['quit', 'Quit Oink'],
  ]) snapshot(name, screen.getByRole('menuitem', { name: label }).querySelector('svg')!);
  cleanup();
  await open({ ...deviceFixture, cursor: 'locked', paused: true, screenSharing: false });
  for (const [name, label] of [['unlock', 'Unlock pointer'], ['resume', 'Resume sharing'], ['show-screens', 'Show all screens']]) {
    snapshot(name, screen.getByRole('menuitem', { name: label }).querySelector('svg')!);
  }
  cleanup();
  const { container } = render(<Glyph name="mouse"/>);
  snapshot('mouse', container.querySelector('svg')!);
});
