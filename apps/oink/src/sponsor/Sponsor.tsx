import { tr, useLanguage } from '../i18n';
import { useEffect, useState } from 'react';
import { Button, Dialog } from 'oink-ui';
import type { DialogProps } from 'oink-ui';
import { fallbackItems, localized, type SponsorItem } from './list';
import { sponsorList } from './remoteList';
import './sponsor.css';

export function useSponsorItems() {
  const [items, setItems] = useState(fallbackItems);
  useEffect(() => {
    let active = true;
    void sponsorList().then(remote => { if (active && remote?.length) setItems(remote); });
    return () => { active = false; };
  }, []);
  return { items, showFallback: () => setItems(fallbackItems) };
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

export function SponsorQrDialog({ item, onOpenChange, container, onImageError }: {
  item: SponsorItem | null; onOpenChange: (open: boolean) => void;
  container: HTMLElement | null; onImageError: () => void;
}) {
  const language = useLanguage();
  if (!item || item.action.type !== 'qr') return null;
  const name = localized(item.name, language);
  const containedProps = { contained: true } as Partial<DialogProps>;
  return (
    <Dialog {...containedProps} open onOpenChange={onOpenChange} container={container} size={320}
      title={name} actions={<Button onClick={() => onOpenChange(false)}>{tr('关闭')}</Button>}>
      <figure className="sponsor-qr">
        <img src={item.action.image} alt={name} width={220} height={220} onError={onImageError} />
        {item.action.caption && <figcaption>{localized(item.action.caption, language)}</figcaption>}
      </figure>
    </Dialog>
  );
}
