import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, copyFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { createHash } from 'node:crypto';
import { auditPackage } from './check-package-licenses.mjs';

for (const platform of ['macos-arm64','windows-x64']) test(`${platform}: package notices must match and new native assets fail`, () => {
  const temp=mkdtempSync(join(tmpdir(),'oink-license-test-'));
  const root=join(temp,'source'), payload=join(temp,'payload');
  const destination=platform==='macos-arm64'?join(payload,'Contents/Resources'):payload;
  const fullText='Copyright 2026 Synthetic Test Authors\nSynthetic test license text\n';
  const sha256=createHash('sha256').update(fullText).digest('hex');
  const files={
    'third-party-licenses.json':JSON.stringify({components:[{licenseFiles:[{path:'licenses/test.txt',sha256}]}]}),
    'licenses/native-inventory.json':JSON.stringify({assets:[{ownership:'first-party',sha256:createHash('sha256').update('synthetic Oink artwork').digest('hex')}],components:[]}),
    'licenses/test.txt':fullText,
    'THIRD_PARTY_LICENSES.md':'synthetic inventory\n','NOTICE':'synthetic notice\n',
    'LICENSE':'synthetic project license\n','LICENSE-ADDITIONAL-TERMS.md':'synthetic additional terms\n',
  };
  try {
    for (const [path,text] of Object.entries(files)) {
      for (const base of [root,destination]) {
        mkdirSync(dirname(join(base,path)),{recursive:true});
        writeFileSync(join(base,path),text);
      }
    }
    writeFileSync(join(destination,'oink.svg'),'synthetic Oink artwork');
    assert.doesNotThrow(()=>auditPackage(payload,platform,root));
    writeFileSync(join(destination,'NOTICE'),'changed notice');
    assert.throws(()=>auditPackage(payload,platform,root),/missing\/different: NOTICE/);
    copyFileSync(join(root,'NOTICE'),join(destination,'NOTICE'));
    writeFileSync(join(destination,'unknown.woff2'),'synthetic unlisted font');
    assert.throws(()=>auditPackage(payload,platform,root),/Uninventoried packaged library\/asset/);
  } finally {
    rmSync(temp,{recursive:true,force:true});
  }
});
