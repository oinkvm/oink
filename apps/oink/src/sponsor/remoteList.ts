import { transport } from '../api/transport';
import type { SponsorItem } from './list';

let result: Promise<SponsorItem[] | null> | undefined;
/** The shell owns all requests and validation. Decode data URIs locally before
 * switching lists, so pending or browser decode failures keep the built-in menu. */
export function sponsorList(): Promise<SponsorItem[] | null> {
  return result ??= load();
}
async function decode(src: string): Promise<void> {
  if (!/^data:image\/(png|jpeg);base64,[A-Za-z0-9+/]+={0,2}$/.test(src) || src.length > 2_800_000) throw new Error('invalid_image');
  const image = new Image();
  image.src = src;
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    await Promise.race([image.decode(), new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error('decode_timeout')), 2000);
    })]);
  } finally { clearTimeout(timer); }
}
async function load(): Promise<SponsorItem[] | null> {
  try {
    const items = await transport.shell<SponsorItem[] | null>('shell_sponsor_list');
    if (!items?.length || items.length > 8) return null;
    const resolved = await Promise.all(items.map(async item => {
      try {
        await decode(item.icon);
        if (item.action.type === 'qr') await decode(item.action.image);
        else if (item.action.type !== 'link' || new URL(item.action.url).protocol !== 'https:') return null;
        return item;
      } catch { return null; }
    }));
    const valid = resolved.filter((item): item is SponsorItem => item !== null);
    return valid.length ? valid : null;
  } catch { return null; }
}
