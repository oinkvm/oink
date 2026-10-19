import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { selectLicense } from './license-policy.mjs';

const allowed = new Set(['MIT','Apache-2.0','Apache-2.0 WITH LLVM-exception','BSD-3-Clause','GPL-3.0-only']);
test('OR chooses an allowed branch without accepting an incompatible alternative', () => {
  assert.equal(selectLicense('GPL-2.0-only OR MIT',allowed),'MIT');
  assert.equal(selectLicense('(GPL-2.0-only AND MIT) OR Apache-2.0',allowed),'Apache-2.0');
});
test('AND requires every term and preserves the chosen obligations', () => {
  assert.throws(()=>selectLicense('MIT AND GPL-2.0-only',allowed));
  assert.equal(selectLicense('(MIT OR Apache-2.0) AND BSD-3-Clause',allowed),'(MIT AND BSD-3-Clause)');
});
test('unknown, malformed and GNU only/or-later expressions do not become permissive', () => {
  for (const expression of ['UNKNOWN','SEE LICENSE IN COPYING','GPL-3.0-or-later','LGPL-2.1-only','MIT OR','LicenseRef-Unknown']) {
    assert.throws(()=>selectLicense(expression,allowed),expression);
  }
  assert.equal(selectLicense('GPL-3.0-only',allowed),'GPL-3.0-only');
});
test('exceptions must be explicitly allowed as a complete license requirement', () => {
  assert.equal(selectLicense('Apache-2.0 WITH LLVM-exception',allowed),'Apache-2.0 WITH LLVM-exception');
  assert.throws(()=>selectLicense('GPL-3.0-only WITH GCC-exception-3.1',allowed));
});

test('component source reviews match exact cargo-deny exceptions', () => {
  const reviews=JSON.parse(readFileSync(new URL('../licenses/compatibility-reviews.json',import.meta.url),'utf8')).reviews;
  const exceptions=readFileSync(new URL('../deny.toml',import.meta.url),'utf8').split('[[licenses.exceptions]]').slice(1);
  for (const review of reviews) {
    assert.ok(exceptions.some(block=>block.match(/^name = "([^"]+)"/m)?.[1]===review.name && block.match(/^version = "([^"]+)"/m)?.[1]===`=${review.version}` && block.match(/allow\s*=\s*\[([^\]]+)\]/s)?.[1].includes(`"${review.license}"`)),`${review.name}@${review.version}: source review/deny mismatch`);
  }
});
