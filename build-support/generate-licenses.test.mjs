import { readFileSync, readdirSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { test } from 'node:test';
import assert from 'node:assert/strict';

const read = path => readFileSync(new URL(`../${path}`, import.meta.url), 'utf8');
const native = JSON.parse(read('licenses/native-inventory.json'));
const inventory = JSON.parse(read('third-party-licenses.json'));
test('native asset inventory matches source bytes and covers backend tray artwork', () => {
  for (const asset of native.assets) {
    const bytes = readFileSync(new URL(`../${asset.path}`, import.meta.url));
    assert.equal(createHash('sha256').update(bytes).digest('hex'), asset.sha256, asset.path);
  }
  const tray = 'crates/oink-agent/src/tray/images/';
  for (const name of readdirSync(new URL(`../${tray}`, import.meta.url)).filter(name => name.endsWith('.png'))) {
    const path = tray + name;
    const asset = native.assets.find(asset => asset.path === path);
    assert.ok(asset, path);
    assert.equal(asset.ownership, 'first-party', path);
    const mac = name.startsWith('mac-');
    assert.deepEqual(asset.scope, [mac ? 'runtime' : 'test'], path);
    assert.deepEqual(asset.platforms, [mac ? 'macos-arm64' : 'windows-x64'], path);
  }
});
test('generator excludes first-party artwork while retaining its package inventory and project license', () => {
  const ownAssets = native.assets.filter(asset => asset.ownership === 'first-party');
  assert.ok(ownAssets.some(asset => asset.path.endsWith('.svg')));
  assert.ok(ownAssets.some(asset => asset.path.endsWith('.bmp')));
  for (const asset of ownAssets) {
    assert.equal(asset.licenseExpression, 'GPL-3.0-only');
    assert.ok(asset.licenseFiles.some(file => file.path === 'LICENSE'));
    assert.match(asset.sha256, /^[a-f0-9]{64}$/);
    assert.ok(!inventory.components.some(component => component.id === asset.id), asset.id);
    assert.ok(!read('THIRD_PARTY_LICENSES.md').includes(asset.name), asset.name);
    assert.ok(!read('NOTICE').includes(`${asset.name} ${asset.version}`), asset.name);
  }
});
test('generator retains every third-party native component and asset', () => {
  for (const component of [...native.components, ...native.assets.filter(asset => asset.ownership !== 'first-party')]) {
    assert.ok(inventory.components.some(item => item.id === component.id), component.id);
  }
  assert.ok(inventory.components.some(component => component.name === 'oink-ui'));
});

test('native menu SVGs are inventoried with their reviewed shape ownership', () => {
  const menu = 'apps/oink/src-tauri/icons/menu/';
  const library = new Set(['lock.svg', 'resume.svg', 'show.svg', 'show-screens.svg']);
  for (const name of readdirSync(new URL(`../${menu}`, import.meta.url)).filter(name => name.endsWith('.svg'))) {
    const asset = native.assets.find(asset => asset.path === menu + name);
    assert.ok(asset, menu + name);
    const expression = name === 'unlock.svg' ? 'GPL-3.0-only AND MIT' : library.has(name) ? 'MIT' : 'GPL-3.0-only';
    assert.equal(asset.licenseExpression, expression, name);
    assert.equal(asset.ownership, expression === 'GPL-3.0-only' ? 'first-party' : 'third-party', name);
    assert.deepEqual(asset.platforms, ['windows-x64'], name);
    assert.deepEqual(asset.scope, ['runtime'], name);
    if (expression.includes('MIT')) {
      assert.ok(asset.licenseFiles.some(file => file.path === 'licenses/native/oink-ui-1.0.5-LICENSE.txt'), name);
    }
  }
});

test('bundled sponsor marks retain project provenance, terms and brand notices', () => {
  for (const name of ['kofi', 'wechat']) {
    const path = `apps/oink/src/sponsor/${name}.svg`;
    const asset = native.assets.find(asset => asset.path === path);
    assert.ok(asset, path);
    assert.equal(asset.ownership, 'first-party', path);
    assert.equal(asset.licenseExpression, 'GPL-3.0-only', path);
    assert.equal(asset.selectedLicense, 'GPL-3.0-only', path);
    assert.deepEqual(asset.platforms, ['macos-arm64', 'windows-x64'], path);
    assert.deepEqual(asset.scope, ['runtime'], path);
    assert.deepEqual(asset.licenseFiles.map(file => file.path), ['LICENSE', 'LICENSE-ADDITIONAL-TERMS.md'], path);
    assert.match(asset.provenance, /Project-authored.*Mark/, path);
    assert.match(asset.trademarkNotice, /grants no trademark rights or endorsement/, path);
  }
});
