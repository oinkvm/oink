import { transport } from '../api/transport';

interface SponsorImage { mime_type: string; base64: string }
const MAX_BYTES = 2 * 1024 * 1024;
let result: Promise<string | null> | undefined;

/** The shell owns the startup request and byte cap; decode locally before use.
 * No remote URL reaches an img element, so the existing self/data CSP suffices. */
export function sponsorQr(): Promise<string | null> {
  return result ??= load();
}

async function load(): Promise<string | null> {
  try {
    const image = await transport.shell<SponsorImage | null>('shell_sponsor_qr');
    if (!image || !['image/png', 'image/jpeg'].includes(image.mime_type) ||
        image.base64.length > Math.ceil(MAX_BYTES / 3) * 4) return null;
    const size = atob(image.base64).length;
    if (!size || size > MAX_BYTES) return null;
    const src = `data:${image.mime_type};base64,${image.base64}`;
    const decoded = new Image();
    decoded.src = src;
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      await Promise.race([
        decoded.decode(),
        new Promise<never>((_, reject) => { timer = setTimeout(() => reject(new Error('decode_timeout')), 2000); }),
      ]);
      return [decoded.naturalWidth, decoded.naturalHeight].every(n => n >= 64 && n <= 2048) ? src : null;
    } finally { clearTimeout(timer); }
  } catch { return null; }
}
