#!/usr/bin/env node
// One inventory for About, source distributions and installed package notices.
// Install: cargo install cargo-about --version 0.9.2 --locked --features cli
//          cargo install cargo-deny --version 0.20.2 --locked
//          npm ci
// Regenerate: node build-support/generate-licenses.mjs
// Drift gate: node build-support/generate-licenses.mjs --check
import { createHash } from 'node:crypto';
import { tmpdir } from 'node:os';
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync, readdirSync, mkdirSync, writeFileSync, unlinkSync, statSync, mkdtempSync, rmSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
// Git's GNU tar can interpret a Windows drive-qualified archive as a remote
// host. Use the OS bsdtar on Windows so npm license archives stay local files.
const tarProgram = process.platform === 'win32' ? join(process.env.SystemRoot,'System32','tar.exe') : 'tar';
process.chdir(root);
const args = process.argv.slice(2);
if (args.includes('--help')) {
  console.log(readFileSync(fileURLToPath(import.meta.url), 'utf8').split('import {')[0]);
  console.log('Optional: --package-root <unpacked directory> --platform macos-arm64|windows-x64');
  console.log('--release-sources rejects the explicitly approved local-beta UI package before public export.');
  process.exit(0);
}
const check = args.includes('--check');
const option = key => args.includes(key) ? args[args.indexOf(key) + 1] : null;
const read = p => readFileSync(p, 'utf8');
const json = p => JSON.parse(read(p));
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const cmp = (a,b) => a < b ? -1 : a > b ? 1 : 0;
const sorted = xs => [...new Set(xs)].sort(cmp);
function run(program, argv) {
  return execFileSync(program, argv, {cwd:root, encoding:'utf8', maxBuffer:128 * 1024 * 1024});
}
const { selectLicense } = await import('./license-policy.mjs');
if (!run('cargo', ['about','--version']).trim().endsWith('0.9.2')) throw Error('Install cargo-about 0.9.2 with --features cli');
if (json('node_modules/spdx-expression-parse/package.json').version !== '5.0.0') throw Error('Run npm ci: spdx-expression-parse 5.0.0 required');
const denyConfig=read('deny.toml');
const policy = denyConfig.split('[licenses]')[1].split('[bans]')[0];
const allowed = new Set([...policy.match(/allow\s*=\s*\[([^\]]+)\]/s)[1].matchAll(/"([^"\n]+)"/g)].map(m=>m[1]));
const select = expr => selectLicense(expr,allowed);
const outputs = new Map();
function textFile(text) {
  const bytes = Buffer.from(text, 'utf8');
  const sha256 = hash(bytes), path = `licenses/texts/${sha256}.txt`;
  outputs.set(path, bytes);
  return {path, sha256};
}
function copyright(texts) {
  return sorted(texts.flatMap(s => s.split('\n').filter(l => /(?:copyright\s*(?:\(c\)|©)?\s*\d{4}|©\s*\d{4})/i.test(l)).map(l=>l.trim())).filter(Boolean));
}
function localNotices(dir, extra = []) {
  const names = readdirSync(dir).filter(n => /(licen[cs]e|copying|notice|copyright|patents|authors)/i.test(n));
  const files = sorted([...names.map(n=>join(dir,n)), ...extra.filter(Boolean).map(n=>resolve(dir,n))]);
  return files.filter(p=>existsSync(p) && statSync(p).isFile()).map(p=>read(p));
}
function base(c, texts) {
  return {...c, copyright:c.copyright ?? copyright(texts), licenseFiles:sorted(texts).map(textFile).sort((a,b)=>cmp(a.path,b.path))};
}
const rustLock = new Map(read('Cargo.lock').split('[[package]]').slice(1).map(block => {
  const field = name => block.match(new RegExp(`^${name} = "([^"\\n]+)"`, 'm'))?.[1];
  return [`${field('name')}@${field('version')}`, field('checksum')];
}));
const metadata = jsonFromRun(['metadata','--locked','--all-features','--format-version','1']);
function jsonFromRun(argv) { return JSON.parse(run('cargo',argv)); }
const packages = new Map(metadata.packages.map(p=>[p.id,p]));
const reviews = json('licenses/compatibility-reviews.json').reviews;
for (const review of reviews) {
  const p = metadata.packages.find(p=>p.name===review.name && p.version===review.version);
  if (!p) continue; // A removed exact-version dependency needs no exception at runtime.
  const exception=denyConfig.split('[[licenses.exceptions]]').slice(1).find(block=>
    block.match(/^name = "([^"]+)"/m)?.[1]===p.name && block.match(/^version = "([^"]+)"/m)?.[1]===`=${p.version}`);
  if (!exception || !exception.match(/allow\s*=\s*\[([^\]]+)\]/s)?.[1].includes(`"${review.license}"`)) throw Error(`Reviewed license/deny exception mismatch: ${p.name}@${p.version}`);
  if (rustLock.get(`${p.name}@${p.version}`) !== review.sourceSha256) throw Error(`Source archive changed: ${p.name}`);
  for (const [path, sha] of Object.entries(review.files)) {
    if (hash(readFileSync(join(dirname(p.manifest_path),path))) !== sha) throw Error(`Reviewed source changed: ${p.name}/${path}`);
  }
}
const platforms = [['macos-arm64','aarch64-apple-darwin'],['windows-x64','x86_64-pc-windows-msvc']];
const reachability = new Map();
for (const [platform,target] of platforms) {
  const m = jsonFromRun(['metadata','--locked','--all-features','--format-version','1','--filter-platform',target]);
  const nodes = new Map(m.resolve.nodes.map(n=>[n.id,n]));
  const queue = m.workspace_members.map(id=>[id,'runtime']);
  const seen = new Set();
  for (let i=0; i<queue.length; ++i) {
    const [id,scope] = queue[i], key = `${id}|${scope}`;
    if (seen.has(key)) continue;
    seen.add(key);
    const info = reachability.get(id) ?? {scope:new Set(), platforms:new Set()};
    info.scope.add(scope); info.platforms.add(platform); reachability.set(id,info);
    for (const dep of nodes.get(id)?.deps ?? []) for (const kind of dep.dep_kinds) {
      const next = kind.kind === 'dev' ? 'test' : kind.kind === 'build' && scope !== 'test' ? 'build' : scope;
      queue.push([dep.pkg,next]);
    }
  }
}
console.log('Collecting full Rust license texts (both targets; runtime/build/test)…');
// cargo-about refuses captured stdout under PowerShell to prevent UTF-16
// corruption. Its explicit output file preserves the same UTF-8 bytes on both
// build platforms and leaves generated metadata outside the source tree.
const aboutTemp = mkdtempSync(join(tmpdir(),'oink-about-'));
let rust;
try {
  const output = join(aboutTemp,'licenses.json');
  run('cargo',['about','generate','--locked','--workspace','--all-features','--fail','--format','json','--output-file',output]);
  rust = JSON.parse(read(output));
} finally {
  rmSync(aboutTemp,{recursive:true,force:true});
}
const components=[];
for (const entry of rust.crates) {
  const p = entry.package;
  if (!p.source) continue;
  const reachable = reachability.get(p.id);
  // A union of target predicates can include a Windows-only child of a
  // macOS-only parent. Keep only packages reachable in a complete target graph.
  if (!reachable) continue;
  if (!p.source.startsWith('registry+https://github.com/rust-lang/crates.io-index')) throw Error(`Unapproved Rust source: ${p.name}`);
  const matching = rust.licenses.filter(l=>l.used_by.some(u=>u.crate.id===p.id));
  const texts=sorted([...matching.map(l=>l.text), ...localNotices(dirname(p.manifest_path),[p.license_file])]);
  if (!texts.length || !matching.length) throw Error(`Missing Rust license text: ${p.name}@${p.version}`);
  const selections = sorted(matching.map(l=>l.id));
  const review=reviews.find(r=>r.name===p.name && r.version===p.version);
  selectLicense(selections.join(' AND '),new Set([...allowed,...(review?[review.license]:[])]));
  components.push(base({id:`cargo:${p.name}@${p.version}`, name:p.name, version:p.version, ecosystem:'cargo',
    repository: httpUrl(p.repository), source:{url:`https://crates.io/api/v1/crates/${p.name}/${p.version}/download`,integrity:`sha256:${rustLock.get(`${p.name}@${p.version}`)}`},
    licenseExpression:entry.license, selectedLicense:selections.join(' AND '), modified:false,
    scope: sorted(reachable?.scope ?? []), platforms:sorted(reachable?.platforms ?? [])},texts));
}
function httpUrl(value) {
  if (!value) return null;
  const url = (typeof value==='string' ? value : value.url).replace(/^git@github.com:/,'https://github.com/').replace(/^github:/,'https://github.com/').replace(/^git\+ssh:\/\/git@github.com\//,'https://github.com/').replace(/^git\+/,'').replace(/^git:\/\//,'https://').replace(/\.git$/,'');
  if (/^[\w.-]+\/[\w.-]+$/.test(url)) return `https://github.com/${url}`;
  return /^https?:\/\//.test(url) ? url : null;
}
console.log('Checking root npm workspace (all lockfile dependency roles)…');
const npmLock=json('package-lock.json');
const npmComponents=new Map();
const npmCache=join(tmpdir(),'oink-license-npm');
async function npmPackageDir(path,locked) {
  const installed=resolve(root,path);
  if (existsSync(join(installed,'package.json'))) return installed;
  if (!locked.integrity || !locked.resolved?.startsWith('https://registry.npmjs.org/')) throw Error(`Untrusted or unpinned npm source: ${path}`);
  const dir=join(npmCache,hash(locked.integrity));
  const archive=join(dir,'source.tgz');
  mkdirSync(dir,{recursive:true});
  if (!existsSync(archive)) {
    console.log(`Fetching optional package license: ${path}`);
    const response=await fetch(locked.resolved,{signal:AbortSignal.timeout(60000)});
    if (!response.ok) throw Error(`npm tarball HTTP ${response.status}: ${path}`);
    writeFileSync(archive,Buffer.from(await response.arrayBuffer()));
  }
  const [algorithm,digest]=locked.integrity.split('-');
  if (!['sha512','sha256','sha1'].includes(algorithm) || createHash(algorithm).update(readFileSync(archive)).digest('base64')!==digest) throw Error(`npm integrity mismatch: ${path}`);
  const listing=run(tarProgram,['-tzf',archive]);
  if (listing.split('\n').filter(Boolean).some(p=>p.startsWith('/') || p.split('/').includes('..'))) throw Error('Unsafe npm archive path');
  run(tarProgram,['-xzf',archive,'-C',dir]);
  return join(dir,'package');
}
for (const [path,locked] of Object.entries(npmLock.packages)) {
  if (!path.includes('node_modules/') || locked.link) continue;
  const dir=await npmPackageDir(path,locked);
  const p=json(join(dir,'package.json')), id=`npm:${p.name}@${p.version}`;
  if (p.version!==locked.version) throw Error(`Installed package/lockfile version differs: ${path}`);
  const localReview=json('licenses/local-npm-sources.json').packages.find(r=>r.name===p.name && r.version===p.version && r.integrity===locked.integrity);
  const localBeta=locked.resolved?.startsWith('file:') && localReview;
  if (localBeta && args.includes('--release-sources')) throw Error(`Local beta dependency must be published before export: ${id}`);
  if (localBeta) {
    const archive=resolve(root,locked.resolved.slice(5));
    const [algorithm,digest]=locked.integrity.split('-');
    if (createHash(algorithm).update(readFileSync(archive)).digest('base64')!==digest) throw Error(`Approved local beta integrity changed: ${id}`);
  } else if (!locked.resolved?.startsWith('https://registry.npmjs.org/') || !locked.integrity) throw Error(`Unapproved/unpinned npm source: ${path}`);
  const roles=locked.dev ? ['build','test'] : ['runtime'];
  if (npmComponents.has(id)) {
    const existing=npmComponents.get(id);
    existing.scope=sorted([...existing.scope,...roles]);
    existing.platforms=sorted([...existing.platforms,...npmPlatforms(locked)]);
    continue;
  }
  const expr=typeof p.license==='string' ? p.license : p.license?.type;
  if (!expr) throw Error(`Missing SPDX license: ${id}`);
  const review=json('licenses/npm-reviews.json').packages.find(r=>r.name===p.name && r.version===p.version && r.licenseExpression===expr);
  if (review && (!locked.dev || review.integrity!==locked.integrity)) throw Error(`npm build-only license review no longer applies: ${id}`);
  const selectedLicense=selectLicense(expr,new Set([...allowed,...(review?.allowed??[])]));
  let texts=localNotices(dir);
  if (!texts.length) {
    const reviewed=json('licenses/npm-text-sources.json').packages.find(r=>r.name===p.name && r.version===p.version && r.licenseExpression===expr);
    if (reviewed) {
      if (json(`node_modules/${reviewed.sourcePackage}/package.json`).version!==reviewed.sourceVersion) throw Error(`npm parent license version changed: ${id}`);
      texts=reviewed.files.map(f=>{
        if (hash(readFileSync(f.path))!==f.sha256) throw Error(`Reviewed npm text changed: ${f.path}`);
        return read(f.path);
      });
    }
  }
  if (!texts.length) throw Error(`Missing npm license text: ${id}`);
  npmComponents.set(id,base({id,name:p.name,version:p.version,ecosystem:'npm',repository:httpUrl(p.repository),
    source:{url:localBeta?localReview.source:locked.resolved,integrity:locked.integrity??null},licenseExpression:expr,selectedLicense,modified:false,
    scope:roles,platforms:npmPlatforms(locked)},texts));
}
components.push(...npmComponents.values());
function npmPlatforms(p) {
  const matches=(list,value)=>!list || (!list.includes(`!${value}`) && (!list.some(x=>!x.startsWith('!')) || list.includes(value)));
  return platforms.map(([platform])=>platform).filter(platform=>matches(p.os,platform==='macos-arm64'?'darwin':'win32') && matches(p.cpu,platform==='macos-arm64'?'arm64':'x64'));
}
const native=json('licenses/native-inventory.json');
for (const c of native.components) {
  const packagingPin = c.name==='Sparkle'
    ? ['build-support/sparkle.py','VERSION','SHA256']
    : c.id==='native:WixToolset.UI.wixext@5.0.2'
      ? ['packaging/windows/build.py','WIX_VERSION','UI_SHA256']
      : c.id==='native:WixToolset.Firewall.wixext@5.0.2'
      ? ['packaging/windows/build.py','WIX_VERSION','FIREWALL_SHA256'] : null;
  if (packagingPin) {
    const source=read(packagingPin[0]);
    const constant=name=>source.match(new RegExp(`^${name} = ['"]([^'"]+)['"]`,'m'))?.[1];
    if (constant(packagingPin[1])!==c.version || constant(packagingPin[2])!==c.archive?.sha256) throw Error(`Native packaging/inventory pin mismatch: ${c.name}`);
  }
  if (c.cargoPackage) {
    const pin=c.cargoPackage;
    const p=metadata.packages.find(p=>p.name===pin.name && p.version===pin.version);
    if (!p) throw Error(`Native inventory pin changed: ${pin.name}@${pin.version}; review bundled Opus before release`);
    for (const [path, sha] of Object.entries(pin.files)) if (hash(readFileSync(join(dirname(p.manifest_path),path)))!==sha) throw Error(`Native source license changed: ${path}`);
  }
  const texts=c.licenseFiles.map(f=>{
    if (hash(readFileSync(f.path))!==f.sha256) throw Error(`Native text hash changed: ${f.path}`);
    return read(f.path);
  });
  // Exact reviewed installer component; never permit MS-RL in the application graph.
  if (['native:WixToolset.Firewall.wixext@5.0.2','native:WixToolset.UI.wixext@5.0.2'].includes(c.id) && c.selectedLicense==='MS-RL' && c.compatibilityReview) {
    const reviewed = c.id==='native:WixToolset.UI.wixext@5.0.2' ? '5ef2c707614b9f70b6bbadd2d4abcb4124efee215e9b16bfbc80113079a604c7' : 'c9a1c99afab82225127b34036b719dea6c98c451e21b341ae1c62a674d0ba5ef';
    if (c.archive?.sha256!==reviewed) throw Error('WiX review pin changed');
  } else select(c.selectedLicense);
  const {archive,cargoPackage,packagePatterns,includedComponents,linkage,patentNotices,compatibilityReview,...common}=c;
  const value=base(common,texts);
  components.push(value);
}
for (const asset of native.assets) {
  if (hash(readFileSync(asset.path))!==asset.sha256) throw Error(`Asset inventory hash changed: ${asset.path}`);
  select(asset.licenseExpression);
  for (const f of asset.licenseFiles) if (hash(readFileSync(f.path))!==f.sha256) throw Error(`Asset license text changed: ${f.path}`);
  if (asset.ownership === 'first-party') continue;
  const {path,sha256,ownership,...component}=asset;
  components.push(base({...component,ecosystem:'asset'},asset.licenseFiles.map(f=>read(f.path))));
}
function scanSourceAssets(dir) {
  if (!existsSync(dir)) return;
  for (const item of readdirSync(dir,{withFileTypes:true})) {
    const path=join(dir,item.name);
    if (item.isDirectory()) scanSourceAssets(path);
    else if (/\.(woff2?|ttf|otf|png|svg|icns|ico|cur|bmp|gif|webp|avif|wav|mp3|ogg|mp4|jpe?g)$/i.test(item.name) && !native.assets.some(a=>a.path===path.replaceAll('\\','/'))) throw Error(`Uninventoried source asset: ${path}`);
  }
}
for (const dir of ['apps/oink/src','apps/oink/public','apps/oink/src-tauri/icons','crates/oink-agent/src/tray','packaging/macos','packaging/windows']) scanSourceAssets(dir);
components.sort((a,b)=>cmp(a.id,b.id));
const inventory={schemaVersion:1,product:'Oink',tools:{cargoAbout:'0.9.2',npmScanner:'lockfile-v1',spdxExpressionParse:'5.0.0'},components};
outputs.set('third-party-licenses.json',Buffer.from(JSON.stringify(inventory,null,2)+'\n'));
const lines=['# Third-party licenses','','Generated by `node build-support/generate-licenses.mjs`; do not edit generated files.','',
  'This inventory audits runtime, build and test dependencies for both product targets. Graph reachability does not assert every package is present in an installer.',
  'The original SPDX expression and selected branch are recorded separately. Upstream copyrights, notices and patent declarations are preserved in the linked texts.',
  'Reviewed Rust MPL-covered source remains available under MPL-2.0 and, when combined with Oink, additionally under GPL-3.0-only pursuant to MPL §3.3. Standalone build tools retain their own licenses. Exact covered sources are linked below; distribute them with release source materials.',
  'WiX firewall custom actions and installer UI are separate MS-RL installer files; their pinned upstream source must accompany release source materials. The WiX CLI itself is a build tool.',
  '','| Component | Selected license | Roles | Platforms | Full texts |','| --- | --- | --- | --- | --- |'];
const escape=s=>s.replaceAll('|','\\|').replaceAll('\n',' ');
for (const c of components) lines.push(`| [${escape(c.name)} ${escape(c.version)}](${c.source.url}) | ${escape(c.selectedLicense)} | ${c.scope.join(', ')} | ${c.platforms.join(', ')} | ${c.licenseFiles.map((f,i)=>`[${i+1}](${f.path})`).join(' ')} |`);
outputs.set('THIRD_PARTY_LICENSES.md',Buffer.from(lines.join('\n')+'\n'));
const notice=['Oink — Oink Is Not a KVM','Project: https://github.com/oinkvm/oink','Contact: builder@hachiwii.com','',
  'Oink original material is licensed GPL-3.0-only. See LICENSE and LICENSE-ADDITIONAL-TERMS.md.',
  'This license does not replace the licenses of third-party components.',
  'Matching Oink source and lockfiles accompany each GitHub Release. Exact dependency sources are linked in third-party-licenses.json.',
  'Reviewed Rust MPL-covered sources are available under MPL-2.0 and additionally GPL-3.0-only in this combined work under MPL section 3.3; their notices remain intact. Standalone build tools retain their own licenses.',
  'WiX firewall custom actions and installer UI remain separate MS-RL installer files. Their corresponding source is at the pinned upstream commit linked in the inventory; provide it with release source materials.',
  'See THIRD_PARTY_LICENSES.md and licenses/ for the complete upstream terms, notices and patent declarations.',''];
for (const c of components) {
  notice.push(`${c.name} ${c.version} (${c.selectedLicense})`,...c.copyright,...c.licenseFiles.map(f=>`Full terms: ${f.path}`),'');
}
outputs.set('NOTICE',Buffer.from(notice.join('\n')+'\n'));
const previous = existsSync('licenses/generated-files.json') ? json('licenses/generated-files.json') : [];
outputs.set('licenses/generated-files.json',Buffer.from(JSON.stringify([...outputs.keys()].sort(cmp),null,2)+'\n'));
let changed=false;
for (const [path,bytes] of outputs) {
  if (!existsSync(path) || !readFileSync(path).equals(bytes)) {
    changed=true;
    if (check) console.error(`Attribution drift: ${path}`);
    else { mkdirSync(dirname(path),{recursive:true}); writeFileSync(path,bytes); }
  }
}
for (const path of previous) if (!outputs.has(path)) {
  if (!/^licenses\/texts\/[a-f0-9]{64}\.txt$/.test(path)) continue;
  changed=true;
  if (check) console.error(`Stale attribution text: ${path}`);
  else if (existsSync(path)) unlinkSync(path);
}
if (check && changed) throw Error('Regenerate and commit the attribution output');
if (option('--package-root')) {
  const { auditPackage } = await import('./check-package-licenses.mjs');
  auditPackage(resolve(option('--package-root')),option('--platform'),root);
}
console.log(`${check?'Verified':'Generated'} attribution for ${components.length} components.`);
