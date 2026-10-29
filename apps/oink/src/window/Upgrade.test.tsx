// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { OinkProvider } from 'oink-ui';
import { UpgradeButton, UpgradeDialog, closeUpdateDialog, openUpdateDialog, releaseNotesForLanguage } from './Upgrade';
import type { UpdatePhase, UpdateStatus } from '../api/update';
import { setLanguage } from '../i18n';
const mock=vi.hoisted(()=>({update:vi.fn(),query:vi.fn(),fail:vi.fn(),openUrl:vi.fn(),receive:null as null|((path:string)=>void),status:null as UpdateStatus|null,boot:null as {open_update_failure?:boolean;platform?:string}|null}));
vi.mock('./PackageDrop',async()=>({...await vi.importActual<typeof import('./PackageDrop')>('./PackageDrop'),usePackageDropReceiver:(_platform:string,receive:(path:string)=>void)=>{mock.receive=receive;}}));
vi.mock('../api/client',()=>({api:{update:mock.update},shell:{openUrl:mock.openUrl}}));
vi.mock('../api/store',()=>({agent:{query:mock.query,fail:mock.fail},useAgent:()=>({update:mock.status,boot:mock.boot})}));
const candidate={build:31,version:'0.1.1',size:1,published_at:null,severity:'normal',notes:{en:'Fixes','zh-Hans':'修复'}};
const base:UpdateStatus={current_build:30,version:'0.1.0',checks_enabled:true,phase:'ready',candidate,checked_at:null,next_check:null,deferred_until:null,blockers:[],progress:100,error:null,scope:'user',can_change_policy:true};
beforeEach(()=>{
  setLanguage('en');vi.clearAllMocks();
  mock.boot=null;
  Object.defineProperty(window,'matchMedia',{configurable:true,value:()=>({matches:true,addEventListener(){},removeEventListener(){}})});
  vi.stubGlobal('ResizeObserver',class{observe(){}unobserve(){}disconnect(){}});
  mock.status={...base,blockers:['audio','remote_input','locked']};
  mock.query.mockImplementation((_key:string,action:()=>unknown)=>action());mock.update.mockResolvedValue({...base,phase:'quiescing'});mock.openUrl.mockResolvedValue(undefined);
});
afterEach(()=>{cleanup();act(()=>closeUpdateDialog());});
// The main window mounts the dialog once and the title bar shows the indicator.
const mainWindow=()=>render(<OinkProvider defaultMotion="reduced"><div className="win"><UpgradeButton/><UpgradeDialog container={null}/></div></OinkProvider>);
const dialog=()=>screen.queryByRole('dialog');
// Visible label without the aria-hidden copy drawn on the progress fill.
const label=(el:Element)=>{const copy=el.cloneNode(true) as Element;copy.querySelectorAll('[aria-hidden="true"]').forEach(n=>n.remove());return copy.textContent;};
const buttons=()=>within(screen.getByRole('dialog')).getAllByRole('button').map(label);
const fill=(el:Element)=>(el.querySelector('.upgrade-progress') as HTMLElement|null)?.style.getPropertyValue('--upgrade-progress');

it('stays closed until a trigger opens it in the main window',()=>{
  mainWindow();
  expect(dialog()).toBeNull();
  act(()=>openUpdateDialog());
  expect(dialog()?.getAttribute('aria-modal')).toBe('true');
  expect(within(dialog()!).getByText('Update to 0.1.1 (31)')).toBeTruthy();
});

it.each([
  ['0.3.0-inhouse.45','0.3.0'],
  ['0.3.0-inhouse.123abcde','0.3.0'],
  ['0.3.0-beta.1','0.3.0-beta.1'],
])('shows candidate version %s with its separate build number', (version,display)=>{
  mock.status={...base,candidate:{...candidate,version,build:45}};
  mainWindow();act(()=>openUpdateDialog());
  expect(within(dialog()!).getByRole('heading').textContent).toBe(`Update to ${display} (45)`);
});

it.each([
  ['zh-Hans','升级到 0.3.0 (45)'],
  ['en','Update to 0.3.0 (45)'],
  ['ja','0.3.0 (45) にアップデート'],
  ['ko','0.3.0 (45) 버전으로 업데이트'],
])('localizes the version/build title in %s', (language,title)=>{
  setLanguage(language);mock.status={...base,candidate:{...candidate,version:'0.3.0-inhouse.45',build:45}};
  mainWindow();act(()=>openUpdateDialog());
  expect(within(dialog()!).getByRole('heading').textContent).toBe(title);
});

it('clicking Install submits once immediately despite active-sharing observations',async()=>{
  mainWindow();act(()=>openUpdateDialog());
  expect(screen.queryByText('Ready to install')).toBeNull();
  expect(screen.getByText('Installation immediately interrupts input, audio and file sharing, then restarts Oink.')).toBeTruthy();
  fireEvent.click(screen.getByRole('button',{name:'Restart to update'}));
  await waitFor(()=>expect(mock.update).toHaveBeenCalledExactlyOnceWith({type:'install',expected_build:31}));
  expect(screen.queryByText('Waiting for idle')).toBeNull();
});
it('reopens the update dialog after rollback without another user click',async()=>{
  mock.boot={open_update_failure:true};mock.status={...base,phase:'rolled_back',error:'transaction'};
  mainWindow();
  await waitFor(()=>expect(dialog()).not.toBeNull());
  expect(within(dialog()!).getByRole('status').textContent).toBe('Previous version restored');
});
it('shows an Install rejection as a failed update when native code restores the window',async()=>{
  mock.update.mockRejectedValueOnce(new Error('install rejected'));
  mainWindow();act(()=>openUpdateDialog());
  fireEvent.click(screen.getByRole('button',{name:'Restart to update'}));
  await waitFor(()=>expect(within(dialog()!).getByRole('status').textContent).toBe('Update failed'));
  expect(within(dialog()!).getByRole('alert').textContent).toContain('Could not complete the update');
});
it('shows a local inspection IPC rejection in the dialog instead of retaining Verifying',async()=>{
  mock.boot={platform:'windows'};mock.status={...base,phase:'verifying',candidate:null};
  mock.update.mockRejectedValueOnce(new Error('connection timed out'));
  mainWindow();act(()=>mock.receive!('C:\\Oink44.msi'));
  await waitFor(()=>expect(within(dialog()!).getByRole('status').textContent).toBe('Update failed'));
  expect(within(dialog()!).getByRole('alert').textContent).toContain('Could not complete the update');
  expect(buttons()).toEqual(['Close']);expect(mock.fail).not.toHaveBeenCalled();
  act(()=>mock.receive!('C:\\Oink-new.msi'));
  await waitFor(()=>expect(within(dialog()!).queryByRole('alert')).toBeNull());
  expect(within(dialog()!).getByRole('status').textContent).toBe('Verifying');
});
it('shows the verification deadline as a terminal dialog error',()=>{
  mock.boot={platform:'windows'};mock.status={...base,phase:'failed',candidate:null,error:'verification_timeout'};
  mainWindow();act(()=>openUpdateDialog());
  expect(within(dialog()!).getByRole('alert').textContent).toBe('Package verification timed out. Oink is still running; please retry.');
  expect(buttons()).toEqual(['Close']);
});

describe('phase content and button labels',()=>{
  // [phase, status text, buttons left to right, extra status fields]
  const cases:[UpdatePhase,string,string[],Partial<UpdateStatus>?][]=[
    ['checking','Checking',['Close'],{candidate:null,progress:null}],
    // Idle says nothing: it does not mean an update is ready, nor that a check ran.
    ['idle','',['Close'],{candidate:null,progress:null,checked_at:1_800_000_000}],
    ['available','Update available',['Cancel','Download update'],{progress:null}],
    ['downloading','Downloading',['Hide','Cancel download'],{progress:42}],
    ['verifying','Verifying',['Hide','Cancel download'],{progress:100}],
    ['ready','',['Cancel','Restart to update']],
    ['quiescing','Preparing update',['Close']],
    ['installing','Installing',['Close']],
    ['awaiting_reboot','Waiting for system restart',['Close']],
    ['installer_busy','Installer is busy',['Close']],
    ['committed','Update installed',['Close']],
    ['rolled_back','Previous version restored',['Close']],
    ['repair_required','Installation repair required',['Close']],
    ['failed','Update failed',['Close'],{error:'network'}],
  ];
  it.each(cases)('%s',(phase,text,labels,extra)=>{
    mock.status={...base,phase,...extra};
    mainWindow();act(()=>openUpdateDialog());
    if(text)expect(within(dialog()!).getByRole('status').textContent).toBe(text);
    else expect(within(dialog()!).queryByRole('status')).toBeNull();
    expect(buttons()).toEqual(labels);
    expect(new Set(buttons()).size).toBe(labels.length);
    if(mock.status.candidate)expect(within(dialog()!).getByText('Fixes')).toBeTruthy();
    if(phase==='failed')expect(within(dialog()!).getByRole('alert').textContent).toBe('Could not complete the update. Retry or install from the releases page.');
  });
  it('manual builds offer the releases page',()=>{
    mock.status={...base,phase:'disabled',candidate:null,progress:null};
    mainWindow();act(()=>openUpdateDialog());
    expect(within(dialog()!).getByText('Download and install updates manually for this version.')).toBeTruthy();
    expect(buttons()).toEqual(['Cancel','View releases']);
    fireEvent.click(screen.getByRole('button',{name:'View releases'}));
    expect(mock.openUrl).toHaveBeenCalledWith('https://github.com/oinkvm/oink/releases');
  });
});

it('Hide leaves the download running and Cancel download aborts it',async()=>{
  mock.status={...base,phase:'downloading',progress:42};
  mainWindow();act(()=>openUpdateDialog());
  fireEvent.click(screen.getByRole('button',{name:'Hide'}));
  await waitFor(()=>expect(dialog()).toBeNull());
  expect(mock.update).not.toHaveBeenCalled();
  // The title-bar progress indicator reopens the dialog.
  fireEvent.click(screen.getByRole('button',{name:'Downloading 42%'}));
  fireEvent.click(within(dialog()!).getByRole('button',{name:'Cancel download'}));
  await waitFor(()=>expect(mock.update).toHaveBeenCalledExactlyOnceWith({type:'cancel'}));
});

it('the title-bar pill downloads directly when available and opens the dialog when ready',async()=>{
  mock.status={...base,phase:'available',progress:null};
  const view=mainWindow();
  fireEvent.click(screen.getByRole('button',{name:'Update to 0.1.1 (31)'}));
  await waitFor(()=>expect(mock.update).toHaveBeenCalledExactlyOnceWith({type:'download'}));
  expect(dialog()).toBeNull();
  mock.status={...base,phase:'ready'};
  view.rerender(<OinkProvider defaultMotion="reduced"><div className="win"><UpgradeButton/><UpgradeDialog container={null}/></div></OinkProvider>);
  fireEvent.click(screen.getByRole('button',{name:'Complete update'}));
  expect(dialog()).toBeTruthy();
});

it('Esc closes without a command and focus returns to the trigger',async()=>{
  mainWindow();
  const trigger=screen.getByRole('button',{name:'Complete update'});
  trigger.focus();fireEvent.click(trigger);
  await waitFor(()=>expect(dialog()?.contains(document.activeElement)).toBe(true));
  fireEvent.keyDown(document.activeElement!,{key:'Escape'});
  await waitFor(()=>expect(dialog()).toBeNull());
  expect(mock.update).not.toHaveBeenCalled();
  await waitFor(()=>expect(document.activeElement).toBe(trigger));
});

describe('progress',()=>{
  const titleBar=()=>document.querySelector('.upgrade--bar')!;
  it('the title-bar indicator shows the percentage, then Verifying with a full bar',()=>{
    mock.status={...base,phase:'downloading',progress:42.4};
    const view=mainWindow();
    expect(label(titleBar())).toBe('42%');
    expect(fill(titleBar())).toBe('42%');
    expect(titleBar().getAttribute('aria-label')).toBe('Downloading 42%');
    // Sparkle restarts its progress at zero for verification (extraction).
    for(const progress of [0,37,100]){
      mock.status={...base,phase:'verifying',progress};
      view.rerender(<OinkProvider defaultMotion="reduced"><div className="win"><UpgradeButton/><UpgradeDialog container={null}/></div></OinkProvider>);
      expect(label(titleBar())).toBe('Verifying');
      expect(fill(titleBar())).toBe('100%');
      expect(titleBar().getAttribute('aria-label')).toBe('Verifying');
    }
  });
  it('the Cancel download button background follows the download and stays full while verifying',()=>{
    mock.status={...base,phase:'downloading',progress:42};
    const view=mainWindow();act(()=>openUpdateDialog());
    const cancel=()=>within(dialog()!).getByRole('button',{name:'Cancel download'});
    expect(fill(cancel())).toBe('42%');
    expect(cancel().querySelector('.upgrade-progress')?.textContent).toBe('Cancel download');
    mock.status={...base,phase:'verifying',progress:5};
    view.rerender(<OinkProvider defaultMotion="reduced"><div className="win"><UpgradeButton/><UpgradeDialog container={null}/></div></OinkProvider>);
    expect(fill(cancel())).toBe('100%');
    expect(within(dialog()!).queryAllByRole('progressbar')).toHaveLength(0);
  });
});

it('selects exact, parent and future update-note languages with English fallback',()=>{
  const notes={en:'English','zh-Hans':'中文',ja:'日本語',ko:'한국어',es:'Español','es-MX':'México',zh:'中文基础'};
  for(const [language,expected] of [['ja-JP','日本語'],['KO-kr','한국어'],['es-MX','México'],['es-AR','Español'],['zh-Hant-TW','中文基础'],['de-DE','English']])
    expect(releaseNotesForLanguage(notes,language)).toBe(expected);
});

for(const [language,text] of [['ja','テスト日本語'],['ko','테스트 한국어']] as const) {
  it(`renders the checked candidate notes in ${language}`,()=>{
    mock.status={...base,candidate:{...candidate,notes:{...candidate.notes,ja:'テスト日本語',ko:'테스트 한국어'}}};
    setLanguage(language);mainWindow();act(()=>openUpdateDialog());
    expect(within(screen.getByRole('dialog')).getByText(text)).toBeTruthy();
    expect(within(screen.getByRole('dialog')).queryByText('Fixes')).toBeNull();
  });
}

for (const phase of ['idle','ready'] as const) {
  it(`retains a relaunched failed-install dialog while service status is ${phase}`,()=>{
    mock.boot={open_update_failure:true};mock.status={...base,phase,error:null};
    mainWindow();
    expect(within(screen.getByRole('dialog')).queryByText('Update failed')).toBeTruthy();
    expect(screen.getByRole('alert')).toBeTruthy();
  });
}

it('consumes a restored failure when dismissed so a later candidate can become Ready',()=>{
  mock.boot={open_update_failure:true};mock.status={...base,phase:'idle',error:null};
  const view=mainWindow();
  expect(within(screen.getByRole('dialog')).getByText('Update failed')).toBeTruthy();
  fireEvent.click(within(screen.getByRole('dialog')).getByRole('button',{name:'Close'}));
  mock.status={...base,phase:'ready'};
  view.rerender(<OinkProvider defaultMotion="reduced"><div className="win"><UpgradeButton/><UpgradeDialog container={null}/></div></OinkProvider>);
  fireEvent.click(screen.getByRole('button',{name:'Complete update'}));
  expect(within(screen.getByRole('dialog')).queryByText('Update failed')).toBeNull();
  expect(within(screen.getByRole('dialog')).getByText('Update to 0.1.1 (31)')).toBeTruthy();
});

// Native URLs, received File Provider paths and materialized promises all use
// this same path selection. Unrelated files never request an updater command.
it('selects exactly one local or received installer path', async()=>{
  const {localPackagePath}=await import('./Upgrade');
  expect(localPackagePath(['/received/Oink.DMG'])).toBe('/received/Oink.DMG');
  expect(localPackagePath(['C:\\received\\Oink.msi'])).toBe('C:\\received\\Oink.msi');
  expect(localPackagePath(['/received/Oink.zip'])).toBeNull();
  expect(localPackagePath(['/one.dmg','/two.dmg'])).toBeNull();
});
it.each(['package_unavailable','wrong_product','wrong_architecture','downgrade','corrupt_package','signature','not_notarized','not_writable','elevation'] as const)('shows a specific local package error for %s',async error=>{
  mock.status={...base,phase:'failed',error};
  openUpdateDialog();render(<UpgradeDialog container={document.body}/>);
  expect(screen.getByRole('alert').textContent).not.toContain('Unable to complete');
  expect(screen.getByRole('alert').textContent?.length).toBeGreaterThan(10);
});

it('uses the same ready dialog and notes for a restored local candidate',()=>{
  mock.boot={platform:'windows'};
  mock.status={...base,phase:'ready',candidate:{...base.candidate!,local_package:true}};
  openUpdateDialog();render(<UpgradeDialog container={document.body}/>);
  expect(screen.getByRole('dialog').textContent).toContain('Update to 0.1.1 (31)');
  expect(screen.getByText('Fixes')).toBeTruthy();
  expect(screen.queryByText(/Windows requests administrator/)).toBeNull();
  expect(screen.getByText(/Oink cannot authenticate this local Windows package/)).toBeTruthy();
});
it('does not offer download cancellation after a local install was authorized',()=>{
  mock.status={...base,phase:'verifying',candidate:{...base.candidate!,local_package:true}};
  openUpdateDialog();render(<UpgradeDialog container={document.body}/>);
  expect(screen.queryByRole('button',{name:'Cancel download'})).toBeNull();
});

it('discards a local confirmation when canceled without requesting installation',async()=>{
  mock.status={...base,phase:'ready',candidate:{...base.candidate!,local_package:true}};
  openUpdateDialog();render(<UpgradeDialog container={document.body}/>);
  fireEvent.click(screen.getByRole('button',{name:'Cancel'}));
  await waitFor(()=>expect(mock.update).toHaveBeenCalledExactlyOnceWith({type:'cancel'}));
  await waitFor(()=>expect(screen.queryByRole('dialog')).toBeNull());
});

it('omits the notes list for a legacy local package',()=>{
  mock.status={...base,candidate:{...candidate,local_package:true,notes:null}};
  mainWindow();act(()=>openUpdateDialog());
  expect(within(screen.getByRole('dialog')).queryByRole('list')).toBeNull();
});

it.each(['idle','committed','disabled'] as const)('does not advertise a retained candidate in %s', phase => {
  mock.status={...base,current_build:candidate.build,phase,candidate:{...candidate,local_package:true}};
  const {container}=mainWindow();
  expect(container.querySelector('.upgrade')).toBeNull();
});

it.each(['policy','storage','not_configured','transaction','busy'] as const)('shows the typed %s refusal from the install IPC', async reason=>{
  const {ApiFailure}=await import('../api/transport');
  mock.status={...base,candidate:{...candidate,local_package:true}};
  mock.update.mockRejectedValueOnce(new ApiFailure({code:'update_failed',update_error:reason,problem:{code:'state_conflict',unknownCode:null,nativeCode:null,retryAfterMs:null}}));
  mainWindow();act(()=>openUpdateDialog());
  fireEvent.click(screen.getByRole('button',{name:'Restart to update'}));
  await waitFor(()=>expect(within(dialog()!).getByRole('status').textContent).toBe('Update failed'));
  const text=within(dialog()!).getByRole('alert').textContent;
  expect(text).not.toContain('Could not complete the update');
  expect(text).not.toContain('Backend service failed');
  expect(mock.update).toHaveBeenCalledExactlyOnceWith({type:'install',expected_build:candidate.build});
  expect(mock.fail).not.toHaveBeenCalled();
});

it('a pointer click inside the local Ready dialog sends Install without dismissing or canceling',async()=>{
  mock.status={...base,candidate:{...candidate,build:65,local_package:true}};
  let reject!:(error:Error)=>void;
  mock.update.mockImplementationOnce(()=>new Promise((_resolve,fail)=>{reject=fail;}));
  const host=document.createElement('div');host.className='win';document.body.appendChild(host);
  openUpdateDialog();const view=render(<UpgradeDialog container={host}/>);
  const button=within(host).getByRole('button',{name:'Restart to update'});
  fireEvent.pointerDown(button,{button:0,pointerType:'mouse'});fireEvent.mouseDown(button,{button:0});
  fireEvent.pointerUp(button,{button:0,pointerType:'mouse'});fireEvent.mouseUp(button,{button:0});fireEvent.click(button);
  await waitFor(()=>expect(mock.update).toHaveBeenCalledExactlyOnceWith({type:'install',expected_build:65}));
  expect(within(host).getByRole('dialog')).toBeTruthy();
  await act(async()=>reject(new Error('native install refusal')));
  expect(mock.update).toHaveBeenCalledTimes(1);
  expect(within(host).getByRole('status').textContent).toBe('Update failed');
  expect(mock.fail).not.toHaveBeenCalled();view.unmount();host.remove();
});
