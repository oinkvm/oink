import { useEffect, useState } from 'react';
import { Button, EmptyState, Icon, OinkProvider, Spinner, Toaster, toast } from 'oink-ui';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { startupKind, startupMessages } from '../i18n/problems';
import { useLanguage, tr } from '../i18n';
import { shell } from '../api/client';
import { transport } from '../api/transport';
import { PauseGlyph } from '../devices/icons';
import { version } from '../version';
import { DesktopWindow } from './DesktopWindow';
import { ResizeEdges } from './ResizeEdges';
import { revealWhenReady } from './ready';

export function StartupPanel({ raw }: { raw: string }) {
  const kind = startupKind(raw);
  const zh = useLanguage() === 'zh-Hans';
  const [titleZh, titleEn, descZh, descEn] = startupMessages[kind];
  const [details, setDetails] = useState<string | null>(null);
  const diagnostic = async () => {
    const latest = await transport.shell<string>('startup_details');
    return [`Oink ${version}`, kind, raw, latest].join('\n');
  };
  const run = (action: () => Promise<unknown>) => void action().catch(() => toast.danger({title: zh ? '操作失败' : 'Action failed'}));
  const retry = ['stopped','disabled','session','update','exited'].includes(kind);
  const download = kind === 'missing' || kind === 'update';
  return <div className="boot-wrap"><div className="boot-content">
    <EmptyState className="boot" data-tone={kind} icon={kind === 'waiting' ? <Spinner/> : kind === 'rdp' ? <PauseGlyph/> : <Icon name={kind === 'session' ? 'user' : kind === 'update' ? 'refresh' : 'alert'}/>}
      title={zh ? titleZh : titleEn} description={zh ? descZh : descEn} actions={<>
        {download && <Button variant="accent" onClick={() => run(() => shell.openUrl('https://github.com/oinkvm/oink/releases/latest'))}><Icon name="download"/>{zh ? '下载安装包' : 'Download installer'}</Button>}
        {retry && <Button variant={download ? undefined : 'accent'} onClick={() => run(() => transport.shell('startup_retry'))}>{zh ? '重试' : 'Retry'}</Button>}
        {(kind === 'disabled' || kind === 'stopped') && <Button onClick={() => run(() => transport.shell('startup_open_services'))}>{zh ? '打开「服务」' : 'Open Services'}</Button>}
      </>}/>
    <div className="boot__tools">
      <Button size="sm" variant="ghost" onClick={() => run(async () => { await navigator.clipboard.writeText(await diagnostic()); toast.success({title:zh ? '诊断信息已复制' : 'Diagnostic info copied'}); })}><Icon name="copy"/>{zh ? '复制诊断信息' : 'Copy diagnostic info'}</Button>
      <Button size="sm" variant="ghost" onClick={() => run(() => transport.shell('startup_open_logs'))}><Icon name="folder"/>{zh ? '打开日志文件夹' : 'Open log folder'}</Button>
      <Button size="sm" variant="ghost" aria-expanded={details !== null} onClick={() => run(async () => setDetails(details === null ? await diagnostic() : null))}>{zh ? '技术详情' : 'Technical details'}<Icon name={details === null ? 'chevron-down' : 'chevron-up'}/></Button>
    </div>{details !== null && <pre className="boot__details">{details}</pre>}
  </div></div>;
}

export function StartupWindow({raw}: {raw: string}) {
  const language = useLanguage();
  const [container, setContainer] = useState<HTMLDivElement | null>(null);
  useEffect(() => revealWhenReady(() => void shell.ready()), []);
  return <OinkProvider locale={language === 'zh-Hans' ? 'zh-CN' : 'en-US'}><div className="oink-app" data-view="main" data-platform="win">
    <div className="win-shadow" aria-hidden="true"><div className="win-shadow__shape"/></div>
    <DesktopWindow ref={setContainer} platform="win" state={{maximized:false,minimized:false,closed:false}} active title={tr('显示器排列')}
      onMinimize={() => void getCurrentWindow().minimize()} onToggleMaximize={() => void getCurrentWindow().toggleMaximize()} onClose={() => void shell.close()}
      onActivate={() => {}} navOpen={false} nav={() => null} onToggleNav={() => {}} onSideAction={action => { if (action === 'quit') void shell.quit(); }} version={version}>
      <div className="disp"><div className="disp__canvas"><StartupPanel key={raw} raw={raw}/></div></div>
    </DesktopWindow><ResizeEdges platform="win"/>{container && <Toaster container={container}/>}
  </div></OinkProvider>;
}
