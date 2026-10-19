#!/usr/bin/env node
// Validate an unpacked payload after generation; does not start or install it.
import { createHash } from 'node:crypto';
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { dirname, join, resolve, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
const hash = bytes => createHash('sha256').update(bytes).digest('hex');

export function auditPackage(packageRoot,platform,root = resolve(dirname(fileURLToPath(import.meta.url)),'..')) {
  const inventory=JSON.parse(readFileSync(join(root,'third-party-licenses.json'),'utf8'));
  const native=JSON.parse(readFileSync(join(root,'licenses/native-inventory.json'),'utf8'));
  if (!['macos-arm64','windows-x64'].includes(platform)) throw Error('--platform must be macos-arm64 or windows-x64');
  const attributionRoot=platform==='macos-arm64' ? join(packageRoot,'Contents/Resources') : packageRoot;
  for (const path of ['third-party-licenses.json','THIRD_PARTY_LICENSES.md','NOTICE','LICENSE','LICENSE-ADDITIONAL-TERMS.md', ...new Set(inventory.components.flatMap(c=>c.licenseFiles.map(f=>f.path)))]) {
    const actual=join(attributionRoot,path);
    if (!existsSync(actual) || !readFileSync(actual).equals(readFileSync(join(root,path)))) throw Error(`Package attribution missing/different: ${path}`);
  }
  const recognized=new Set(native.assets.map(a=>a.sha256));
  for (const f of inventory.components.flatMap(c=>c.licenseFiles)) recognized.add(f.sha256);
  function walk(dir) {
    for (const name of readdirSync(dir,{withFileTypes:true})) {
      if (name.isSymbolicLink()) continue;
      const file=join(dir,name.name), rel=relative(packageRoot,file).replaceAll('\\','/');
      if (name.isDirectory()) { walk(file); continue; }
      if (/Sparkle\.framework\//.test(rel) && native.components.some(c=>c.name==='Sparkle')) continue;
      // Runtime/embedded-resource manifests still require packaging-owner review.
      // This gate rejects new standalone native libraries and media/font assets.
      if (/\.(dll|dylib|so|woff2?|ttf|otf|png|svg|icns|ico|cur|bmp|gif|webp|avif|wav|mp3|ogg|mp4|jpe?g)$/i.test(name.name) && !recognized.has(hash(readFileSync(file)))) throw Error(`Uninventoried packaged library/asset: ${rel}`);
    }
  }
  walk(packageRoot);
}

if (process.argv[1] && resolve(process.argv[1])===fileURLToPath(import.meta.url)) {
  if (process.argv.length!==4) throw Error('Usage: node build-support/check-package-licenses.mjs <unpacked-root> <macos-arm64|windows-x64>');
  auditPackage(resolve(process.argv[2]),process.argv[3]);
  console.log('Package attribution and standalone native assets verified.');
}
