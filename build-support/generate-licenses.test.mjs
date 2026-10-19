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
