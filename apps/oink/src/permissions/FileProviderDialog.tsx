import { useRef, useState } from 'react';
import { Button, Dialog } from 'oink-ui';
import type { DialogProps } from 'oink-ui';
import { tr } from '../i18n';

const contained = { contained: true } as Partial<DialogProps>;

/**
 * Enabling the File Provider happens in a Finder window the user may not expect,
 * with the Enable button in its top-right corner. App's grant path for `files`
 * asks first: `ask()` resolves true on OK and false on Cancel/close, and only OK
 * goes on to open Finder. Settings and onboarding share this one step.
 */
export function useFileProviderExplainer() {
  const [open, setOpen] = useState(false);
  const resolve = useRef<((ok: boolean) => void) | null>(null);
  const answer = (ok: boolean) => { resolve.current?.(ok); resolve.current = null; setOpen(false); };
  return {
    ask: () => new Promise<boolean>(r => { resolve.current?.(false); resolve.current = r; setOpen(true); }),
    dialog: { open, onAnswer: answer },
  };
}

export function FileProviderDialog({ open, onAnswer, container }: { open: boolean; onAnswer: (ok: boolean) => void; container: HTMLElement | null }) {
  return <Dialog {...contained} open={open} onOpenChange={o => { if (!o) onAnswer(false); }} container={container} size={360}
    title={tr('启用文件提供服务')} description={tr('请在弹出的 Finder 窗口右上角点击“启用”')}
    actions={<>
      <Button onClick={() => onAnswer(false)}>{tr('取消')}</Button>
      <Button variant="accent" onClick={() => onAnswer(true)}>{tr('确定')}</Button>
    </>}/>;
}
