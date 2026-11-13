import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mavenComponents } from './maven-licenses.mjs';

const read = path => readFileSync(new URL(`../${path}`, import.meta.url));
const policy = JSON.parse(read('licenses/maven-licenses.json'));
const allowed = new Set(['MIT', 'Apache-2.0', 'BSD-3-Clause']);
const sha = text => createHash('sha256').update(Buffer.from(text, 'utf8')).digest('hex');
const apache = read('licenses/maven/Apache-2.0.txt').toString('utf8');

function module(group, name, version, pomLicenses, legal = []) {
  return {group, name, version, artifacts: [{file: `${name}-${version}.${group.startsWith('androidx') ? 'aar' : 'jar'}`, sha256: '0'.repeat(64)}],
    pomLicenses, licensesFrom: `${group}:${name}:${version}`, url: null, scm: null,
    legalFiles: legal.map(([path, text]) => ({artifact: `${name}-${version}.jar`, path, sha256: sha(text)}))};
}
function inventory(...components) {
  const texts = {};
  for (const [path, text] of components.flatMap(c => c.legal ?? [])) texts[sha(text)] = text;
  return {schemaVersion: 1, configuration: 'releaseRuntimeClasspath', components: components.map(c => c.module), texts};
}
const entry = (group, name, version, licenses, legal) => ({module: module(group, name, version, licenses, legal), legal});
const run = gradle => mavenComponents(gradle, policy, allowed, path => read(path));

// What v040-android-viewer adds (AndroidX Media3 + Guava): when the merged build's
// `:app:oinkLicenseInventory` writes these modules, the generator takes them without any change here.
// POM names as Media3 and Guava publish them; fixture data, not the committed graph.
const notice = 'Guava fixture NOTICE\nCopyright (C) 2026 The Guava Authors\n';
const mit = 'The MIT License\n\nCopyright 2004-present by the Checker Framework developers\n\nPermission is hereby granted...\n';
const viewer = inventory(
  entry('androidx.media3', 'media3-exoplayer', '1.8.0', [{name: 'The Apache Software License, Version 2.0', url: 'http://www.apache.org/licenses/LICENSE-2.0.txt'}]),
  entry('androidx.media3', 'media3-common', '1.8.0', [{name: 'The Apache Software License, Version 2.0', url: 'http://www.apache.org/licenses/LICENSE-2.0.txt'}]),
  entry('com.google.guava', 'guava', '33.4.8-android', [{name: 'Apache License, Version 2.0', url: 'http://www.apache.org/licenses/LICENSE-2.0.txt'}], [['META-INF/NOTICE', notice]]),
  entry('com.google.guava', 'failureaccess', '1.0.3', [{name: 'The Apache Software License, Version 2.0', url: 'http://www.apache.org/licenses/LICENSE-2.0.txt'}]),
  entry('com.google.errorprone', 'error_prone_annotations', '2.36.0', [{name: 'Apache 2.0', url: 'http://www.apache.org/licenses/LICENSE-2.0.txt'}]),
  entry('org.checkerframework', 'checker-qual', '3.43.0', [{name: 'The MIT License', url: 'http://opensource.org/licenses/MIT'}], [['META-INF/LICENSE.txt', mit]]),
);

test('Media3 and Guava are inventoried as Apache-2.0 with the full text and shipped NOTICE', () => {
  const components = run(viewer);
  const byId = id => components.find(c => c.id === id);
  for (const id of ['maven:androidx.media3:media3-exoplayer@1.8.0', 'maven:androidx.media3:media3-common@1.8.0', 'maven:com.google.guava:guava@33.4.8-android',
    'maven:com.google.guava:failureaccess@1.0.3', 'maven:com.google.errorprone:error_prone_annotations@2.36.0']) {
    assert.equal(byId(id).selectedLicense, 'Apache-2.0', id);
    assert.ok(byId(id).texts.includes(apache), id);
  }
  assert.ok(byId('maven:com.google.guava:guava@33.4.8-android').texts.includes(notice));
  assert.match(byId('maven:androidx.media3:media3-exoplayer@1.8.0').source.url, /^https:\/\/dl\.google\.com\/dl\/android\/maven2\/androidx\/media3\/media3-exoplayer\/1\.8\.0\//);
  assert.match(byId('maven:com.google.guava:guava@33.4.8-android').source.url, /^https:\/\/repo1\.maven\.org\/maven2\/com\/google\/guava\/guava\//);
  // MIT has no reviewed canonical text: the artifact's own license (with its copyright) is used.
  assert.deepEqual(byId('maven:org.checkerframework:checker-qual@3.43.0').texts, [mit]);
});

test('unknown, disallowed and ambiguous POM licenses fail the check', () => {
  assert.throws(() => run(inventory(entry('org.example', 'a', '1', [{name: 'Example Public License', url: null}]))), /Unknown Maven POM license/);
  const gpl = {...policy, names: {...policy.names, 'GNU General Public License, version 2': 'GPL-2.0-only'}};
  assert.throws(() => mavenComponents(inventory(entry('org.example', 'b', '1', [{name: 'GNU General Public License, version 2', url: null}])), gpl, allowed, read),
    /not allowed: GPL-2.0-only/);
  assert.throws(() => run(inventory(entry('org.example', 'c', '1', [{name: 'Apache-2.0', url: null}, {name: 'MIT', url: null}]))), /declares 2 licenses/);
  assert.throws(() => run(inventory(entry('org.example', 'd', '1', [{name: 'MIT', url: null}]))), /No license text for MIT/);
  assert.throws(() => run(inventory(entry('org.example', 'e', '1', []))), /declares 0 licenses/);
});

test('a reviewed multi-license module must match its POM exactly', () => {
  const review = policy.reviews.find(r => r.name === 'jna');
  const jna = pom => inventory(entry(review.group, review.name, review.version, pom, [['classes.jar!/META-INF/LICENSE', 'SPDX-License-Identifier: Apache-2.0 OR LGPL-2.1-or-later\n']]));
  assert.equal(run(jna(review.pomLicenses))[0].selectedLicense, 'Apache-2.0');
  assert.throws(() => run(jna([review.pomLicenses[0]])), /review no longer matches/);
});

test('edited legal texts fail the hash check', () => {
  const gradle = structuredClone(viewer);
  const key = Object.keys(gradle.texts)[0];
  gradle.texts[key] += 'edited';
  assert.throws(() => run(gradle), /legal text hash changed/);
});

test('the committed Android graph is inventoried, listed for the Android app and not for iOS or desktop', () => {
  const gradle = JSON.parse(read('licenses/android-runtime.json'));
  const inventory = JSON.parse(read('third-party-licenses.json'));
  const android = JSON.parse(read('licenses/mobile-android.json'));
  const ios = JSON.parse(read('licenses/mobile-ios.json'));
  assert.ok(gradle.components.some(m => m.group === 'androidx.compose.ui'));
  for (const m of gradle.components) {
    const id = `maven:${m.group}:${m.name}@${m.version}`;
    const component = inventory.components.find(c => c.id === id);
    assert.ok(component, id);
    assert.deepEqual(component.platforms, ['android-arm64', 'android-x64'], id);
    assert.ok(android.components.some(c => c.id === id), id);
    assert.ok(!ios.components.some(c => c.id === id), id);
  }
  for (const app of [android, ios]) for (const c of app.components) for (const text of c.texts) read(`licenses/texts/${text}.txt`);
  assert.ok(ios.components.some(c => c.id.startsWith('cargo:uniffi@')));
  assert.ok(!android.components.some(c => c.id.startsWith('npm:')));
});
