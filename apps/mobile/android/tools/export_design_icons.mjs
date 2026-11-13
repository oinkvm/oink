// Converts the mobile design mockup's SVG icons into Kotlin path data for Compose ImageVectors.
// Sources (all read from the mockup checkout, never redrawn by hand):
//   src/pc-icons-data.js   PC device glyphs, badges, OS logos and action icons
//   src/android-head.js    official Android robot head (CC BY 3.0, see licenses screen) and the privileged-mode crown
//   lucide-react           the lucide icons the mockup imports, rendered to SVG markup
//   src/MobileApp.jsx      the keyboard+mouse tab icon (mask becomes a clip path)
//   src/TransferPage.jsx   the image placeholder artwork
// usage: node tools/export_design_icons.mjs /path/to/oink-mobile-ui
import { writeFileSync, readFileSync } from 'node:fs';
import { createRequire } from 'node:module';

const mock = process.argv[2];
if (!mock) { console.error('usage: export_design_icons.mjs <oink-mobile-ui>'); process.exit(2); }
const req = createRequire(`${mock}/package.json`);
const React = req('react');
const { renderToStaticMarkup } = req('react-dom/server');
const L = await import(req.resolve('lucide-react'));
const pc = await import(`${mock}/src/pc-icons-data.js`);
const head = await import(`${mock}/src/android-head.js`);

const n = v => Number(v);
const fmt = v => String(+(+v).toFixed(4));
function rectPath(x, y, w, h, rx = 0, ry = rx) {
  x = n(x); y = n(y); w = n(w); h = n(h); rx = Math.min(n(rx), w / 2); ry = Math.min(n(ry), h / 2);
  if (!rx || !ry) return `M${fmt(x)} ${fmt(y)}H${fmt(x + w)}V${fmt(y + h)}H${fmt(x)}Z`;
  return `M${fmt(x + rx)} ${fmt(y)}H${fmt(x + w - rx)}A${fmt(rx)} ${fmt(ry)} 0 0 1 ${fmt(x + w)} ${fmt(y + ry)}V${fmt(y + h - ry)}A${fmt(rx)} ${fmt(ry)} 0 0 1 ${fmt(x + w - rx)} ${fmt(y + h)}H${fmt(x + rx)}A${fmt(rx)} ${fmt(ry)} 0 0 1 ${fmt(x)} ${fmt(y + h - ry)}V${fmt(y + ry)}A${fmt(rx)} ${fmt(ry)} 0 0 1 ${fmt(x + rx)} ${fmt(y)}Z`;
}
// Same rounded rectangle, drawn counter-clockwise (used to punch a hole in a clip path).
function rectPathCcw(x, y, w, h, r) {
  return `M${fmt(x + r)} ${fmt(y)}A${fmt(r)} ${fmt(r)} 0 0 0 ${fmt(x)} ${fmt(y + r)}V${fmt(y + h - r)}A${fmt(r)} ${fmt(r)} 0 0 0 ${fmt(x + r)} ${fmt(y + h)}H${fmt(x + w - r)}A${fmt(r)} ${fmt(r)} 0 0 0 ${fmt(x + w)} ${fmt(y + h - r)}V${fmt(y + r)}A${fmt(r)} ${fmt(r)} 0 0 0 ${fmt(x + w - r)} ${fmt(y)}Z`;
}
const ellipsePath = (cx, cy, rx, ry) => { cx = n(cx); cy = n(cy); rx = n(rx); ry = n(ry); return `M${fmt(cx - rx)} ${fmt(cy)}A${fmt(rx)} ${fmt(ry)} 0 1 0 ${fmt(cx + rx)} ${fmt(cy)}A${fmt(rx)} ${fmt(ry)} 0 1 0 ${fmt(cx - rx)} ${fmt(cy)}Z`; };
const attrs = s => Object.fromEntries([...s.matchAll(/([a-zA-Z0-9:-]+)="([^"]*)"/g)].map(m => [m[1], m[2]]));

/** Parses SVG body markup into parts: { d, role, width?, color? }. */
function parts(body, defaultRole = 'S') {
  const out = [];
  for (const m of body.matchAll(/<(path|rect|circle|ellipse|line|polyline|polygon)\b([^>]*?)\/?>/g)) {
    const a = attrs(m[2]); let d;
    switch (m[1]) {
      case 'path': d = a.d; break;
      case 'rect': d = rectPath(a.x || 0, a.y || 0, a.width, a.height, a.rx ?? a.ry ?? 0, a.ry ?? a.rx ?? 0); break;
      case 'circle': d = ellipsePath(a.cx || 0, a.cy || 0, a.r, a.r); break;
      case 'ellipse': d = ellipsePath(a.cx || 0, a.cy || 0, a.rx, a.ry); break;
      case 'line': d = `M${a.x1} ${a.y1}L${a.x2} ${a.y2}`; break;
      default: { const p = a.points.trim().split(/[\s,]+/); d = `M${p[0]} ${p[1]}` + p.slice(2).reduce((s, v, i) => s + (i % 2 ? ` ${v}` : `L${v}`), '') + (m[1] === 'polygon' ? 'Z' : ''); }
    }
    const cls = a.class || '';
    let role = defaultRole, width, color;
    if (cls.includes('knock') || cls.includes('keyhole')) role = 'K';
    else if (cls.includes('warn-body')) role = 'W';
    else if (cls.includes('warn-mark')) { role = 'M'; width = 1.2; }
    else if (cls.includes('shackle')) { role = 'S'; width = 1.7; }
    else if (cls.includes('fill')) role = 'F';
    else if (cls.includes('dim')) role = 'D';
    if (a.fill && a.fill !== 'none' && a.fill !== 'currentColor') { role = 'C'; color = a.fill; }
    if (a.fill === 'none' && a.stroke === 'currentColor') { role = 'S'; width = a['stroke-width'] ? n(a['stroke-width']) : undefined; }
    if (a.fill === 'currentColor') role = 'F';
    out.push({ d, role, width, color });
  }
  return out;
}

const icons = {};
const add = (name, viewBox, list, clip) => { icons[name] = { viewBox: viewBox.split(/[\s,]+/).map(Number), parts: list, clip }; };

// PC glyphs (stroke rules from pc-icons.css: glyph 1.4, badge 1.3, action 1.8; applied at draw time).
for (const [k, body] of Object.entries(pc.GLYPHS)) add(`pc_${k}`, '0 0 16 16', parts(body));
for (let w = 0; w <= 4; w++) { add(`pc_speaker_${w}`, '0 0 16 16', parts(pc.speakerGlyph(w, false))); add(`pc_speaker_${w}_muted`, '0 0 16 16', parts(pc.speakerGlyph(w, true))); }
for (const [k, body] of Object.entries(pc.BADGES)) add(`badge_${k}`, '-1 -1 12 12', parts(pc.BADGE_KNOCK + body));
for (const [k, o] of Object.entries(pc.OS)) add(`os_${k}`, o.viewBox, parts(o.body, 'F'));
for (const [k, body] of Object.entries(pc.ACTIONS)) add(`action_${k}`, '0 0 20 20', parts(body));
pc.SIGNAL_BARS.forEach((b, i) => add(`signal_bar_${i}`, '0 0 14 13', [{ d: rectPath(b.x, b.y, 3, b.h, 0.9), role: 'F' }]));
add('android_head', head.ANDROID_HEAD_VIEWBOX, [{ d: head.ANDROID_HEAD, role: 'F' }, { d: head.ANDROID_EYES, role: 'K' }]);
// Privileged-mode badge: the same head in a viewBox 24 units taller above and below, plus the
// crown in two layers: E = path + round stroke in the crown edge colour (the footprint),
// G = bare path in the crown colour.
add('android_head_crown', head.ANDROID_CROWN_VIEWBOX, [{ d: head.ANDROID_HEAD, role: 'F' }, { d: head.ANDROID_EYES, role: 'K' },
  { d: head.ANDROID_CROWN, role: 'E', width: head.ANDROID_CROWN_STROKE }, { d: head.ANDROID_CROWN, role: 'G' }]);

// Lucide icons used by the mockup (names as imported in src/*.jsx).
const LUCIDE = ['Settings', 'Plus', 'Monitor', 'ArrowDownUp', 'ChevronLeft', 'ChevronRight', 'Play', 'LockKeyhole', 'ArrowRight', 'Camera', 'Image', 'File', 'Folder', 'Check', 'Download',
  'Shield', 'ShieldCheck', 'ShieldAlert', 'Smartphone', 'Wifi', 'KeyRound', 'Info', 'ClipboardCopy', 'ArrowDown', 'ExternalLink', 'X', 'RefreshCw', 'AlertCircle', 'Radar', 'MonitorSmartphone', 'Copy',
  // Transfer page batches and long / password text (TransferBatch.jsx).
  'ChevronsUpDown', 'ChevronsDownUp', 'ChevronDown', 'ChevronUp', 'FileText', 'FileSpreadsheet', 'FileArchive',
  // Transfer composer with no other device online (p2p, 2026-11-11).
  'Ban',
  // Image bubble notices (expired / offline).
  'ImageOff', 'WifiOff',
  // File cards: retry a failed download (2026-11-11).
  'RotateCw'];
for (const name of LUCIDE) {
  const svg = renderToStaticMarkup(React.createElement(L[name], { size: 24 }));
  add(`lucide_${name.replace(/[A-Z]/g, (c, i) => (i ? '_' : '') + c.toLowerCase())}`, '0 0 24 24', parts(svg.replace(/^<svg[^>]*>/, '')));
}

// Keyboard + mouse tab icon (MobileApp.jsx KeyboardMouseIcon). The SVG mask hides the
// keyboard where the mouse sits; the same region becomes a hole in a group clip path.
{
  const src = readFileSync(`${mock}/src/MobileApp.jsx`, 'utf8');
  const fn = src.slice(src.indexOf('function KeyboardMouseIcon'), src.indexOf('const tabs'));
  const masked = fn.slice(fn.indexOf('<g mask'), fn.indexOf('</g>'));
  const tail = fn.slice(fn.indexOf('</g>'), fn.indexOf('</svg>'));
  const hole = attrs(fn.match(/<rect x="15"[^>]*fill="black"\/>/)[0]);
  const clip = `M0 0H24V24H0Z` + rectPathCcw(n(hole.x), n(hole.y), n(hole.width), n(hole.height), n(hole.rx));
  add('tab_keyboard_mouse', '0 0 24 24', [...parts(masked).map(p => ({ ...p, clipped: true })), ...parts(tail)], clip);
}
// iPhone / iPad corner badge drawn over the Apple logo (MobileApp.jsx PlatformIcon): 16 × 20,
// stroke 1.6 with SVG's default butt caps.
{
  const src = readFileSync(`${mock}/src/MobileApp.jsx`, 'utf8');
  const m = src.match(/<rect x=\{platform==='iphone'\?(\d+):(\d+)\} y="(\d+)" width=\{platform==='iphone'\?(\d+):(\d+)\} height="(\d+)" rx="([\d.]+)"\/>\{platform==='iphone'\?<path d="([^"]+)"\/>:<path d="([^"]+)"\/>\}/);
  const [, xi, xp, y, wi, wp, h, rx, pi, pp] = m;
  add('corner_iphone', '0 0 16 20', [{ d: rectPath(xi, y, wi, h, rx), role: 'B' }, { d: pi, role: 'B' }]);
  add('corner_ipad', '0 0 16 20', [{ d: rectPath(xp, y, wp, h, rx), role: 'B' }, { d: pp, role: 'B' }]);
}
// Transfer image placeholder artwork (TransferPage.jsx PreviewImage), fixed colours.
{
  const src = readFileSync(`${mock}/src/TransferPage.jsx`, 'utf8');
  const fn = src.slice(src.indexOf('function PreviewImage'), src.indexOf('function TransferProgress'));
  add('preview_image', '0 0 260 146', parts(fn.slice(fn.indexOf('<rect'), fn.indexOf('</svg>')), 'C'));
}

const kt = s => JSON.stringify(s).replace(/\$/g, '\\$');
let outKt = `// Generated by tools/export_design_icons.mjs from the mobile design mockup. Do not edit.
package com.hachiwii.oink.ui

internal object DesignIconData {
`;
for (const [name, icon] of Object.entries(icons)) {
  const camel = name.replace(/_([a-z0-9])/g, (_, c) => c.toUpperCase());
  const ps = icon.parts.map(p => `IconPart(${kt(p.d)}, '${p.role}'${p.width ? `, ${p.width}f` : ', 0f'}${p.color ? `, 0xFF${p.color.replace('#', '').toUpperCase().padStart(6, '0')}` : ', 0'}${p.clipped ? ', true' : ''})`).join(',\n        ');
  outKt += `    val ${camel} = IconData(${icon.viewBox.map(v => v + 'f').join(', ')}, listOf(\n        ${ps}),${icon.clip ? ` clip = ${kt(icon.clip)}` : ''})\n`;
}
outKt += '}\n';
const target = new URL('../app/src/main/java/com/hachiwii/oink/ui/DesignIconData.kt', import.meta.url);
writeFileSync(target, outKt);
console.log(`${Object.keys(icons).length} icons → ${target.pathname}`);
