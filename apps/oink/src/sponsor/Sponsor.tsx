import { tr } from '../i18n';
import { useEffect, useState } from 'react';
import { shell } from '../api/client';
import { agent } from '../api/store';
import { Button, Dialog } from 'oink-ui';
import type { DialogProps } from 'oink-ui';
import wechatQr from './wechat-qr.png';
import { sponsorQr } from './remoteQr';
import './sponsor.css';

export const KOFI_URL = 'https://ko-fi.com/hachiwii';
export const BRAND = { wechat: '#07C160', kofi: '#29ABE0' } as const;

export function WeChatPayMark({ tile = false }: { tile?: boolean }) {
  return (
    <svg className="sponsor-mark" data-tile={tile ? '' : undefined} viewBox="0 0 20 20" aria-hidden>
      {tile && <rect width="20" height="20" rx="5" fill={BRAND.wechat} />}
      <g transform={tile ? 'translate(2.4 2.6) scale(.76)' : undefined}>
        <path d="M10 2.4c4.5 0 8.1 3.1 8.1 7s-3.6 7-8.1 7c-1 0-1.9-.13-2.8-.4l-3 1.6.75-2.8C3.2 13.5 1.9 11.6 1.9 9.4c0-3.9 3.6-7 8.1-7Z" fill="#fff" />
        <path d="m5.9 9.4 2.5 2.1 5.7-4.4" fill="none" stroke={BRAND.wechat} strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round" />
      </g>
    </svg>
  );
}

export function KofiMark({ tile = false }: { tile?: boolean }) {
  return (
    <svg className="sponsor-mark" data-tile={tile ? '' : undefined} viewBox="0 0 20 20" aria-hidden>
      {tile && <rect width="20" height="20" rx="5" fill={BRAND.kofi} />}
      <g transform={tile ? 'translate(2.4 2.6) scale(.76)' : undefined}>
        <path d="M2.6 5.6h11.3v5.6a4.6 4.6 0 0 1-4.6 4.6H7.2a4.6 4.6 0 0 1-4.6-4.6Z" fill="#fff" />
        <path d="M13.9 7.2h1.3a2.6 2.6 0 0 1 0 5.2h-1.6" fill="none" stroke="#fff" strokeWidth="1.8" strokeLinecap="round" />
        <path d="M8.25 13.1c-.2-.17-2.6-1.9-2.6-3.45 0-.9.67-1.55 1.45-1.55.5 0 .9.25 1.15.65.25-.4.65-.65 1.15-.65.78 0 1.45.65 1.45 1.55 0 1.55-2.4 3.28-2.6 3.45Z" fill="#FF5E5B" />
      </g>
    </svg>
  );
}

export function CoffeeIcon() {
  return (
    <svg className="win__promo-logo" viewBox="0 0 16 16" aria-hidden>
      <path d="M1.4 6.2h10.2v3.9a4.1 4.1 0 0 1-4.1 4.1H5.5a4.1 4.1 0 0 1-4.1-4.1Z" />
      <path d="M11.6 7.4h1a2.05 2.05 0 0 1 0 4.1h-1.2" fill="none" stroke="currentColor" strokeWidth="1.6" />
      <path d="M4.6 1.6c-.65.8-.65 1.6 0 2.4M7.6 1.6c-.65.8-.65 1.6 0 2.4" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" />
    </svg>
  );
}

export const openKofi = () => shell.openUrl(KOFI_URL).catch(() => agent.fail('unavailable'));

export function WeChatQrDialog({ open, onOpenChange, container }: { open: boolean; onOpenChange: (o: boolean) => void; container: HTMLElement | null }) {
  const [remoteQr, setRemoteQr] = useState<string | null>(null);
  useEffect(() => {
    let active = true;
    void sponsorQr().then(src => { if (active) setRemoteQr(src); });
    return () => { active = false; };
  }, []);
  const containedProps = { contained: true } as Partial<DialogProps>;
  return (
    <Dialog
      {...containedProps}
      open={open} onOpenChange={onOpenChange} container={container} size={320}
      title={tr('微信支付')}
      actions={<Button onClick={() => onOpenChange(false)}>{tr('关闭')}</Button>}
    >
      <figure className="sponsor-qr">
        <img src={remoteQr ?? wechatQr} alt={tr('微信支付收款二维码')} width={220} height={220} onError={() => setRemoteQr(null)} />
        <figcaption>{tr('打开微信，扫一扫')}</figcaption>
      </figure>
    </Dialog>
  );
}
