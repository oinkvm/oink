// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { OinkProvider } from 'oink-ui';
import { UpgradeButton, UpgradeDialog, closeUpdateDialog, openUpdateDialog, releaseNotesForLanguage } from './Upgrade';
import type { UpdatePhase, UpdateStatus } from '../api/update';
import { setLanguage } from '../i18n';
const mock=vi.hoisted(()=>({update:vi.fn(),query:vi.fn(),fail:vi.fn(),openUrl:vi.fn(),status:null as UpdateStatus|null}));
vi.mock('../api/client',()=>({api:{update:mock.update},shell:{openUrl:mock.openUrl}}));
vi.mock('../api/store',()=>({agent:{query:mock.query,fail:mock.fail},useAgent:()=>({update:mock.status})}));
const candidate={build:31,version:'0.1.1',size:1,published_at:null,severity:'normal',notes:{en:'Fixes','zh-Hans':'修复'}};
const base:UpdateStatus={current_build:30,version:'0.1.0',channel:'stable',checks_enabled:true,phase:'ready',candidate,checked_at:null,next_check:null,deferred_until:null,blockers:[],progress:100,error:null,scope:'user',can_change_policy:true};
beforeEach(()=>{
  setLanguage('en');vi.clearAllMocks();
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
  expect(within(dialog()!).getByText('Update to v0.1.1')).toBeTruthy();
});

it('clicking Install submits once immediately despite active-sharing observations',async()=>{
  mainWindow();act(()=>openUpdateDialog());
  expect(screen.getByText('Ready to install')).toBeTruthy();
  expect(screen.getByText('Installation immediately interrupts input, audio and file sharing, then restarts Oink.')).toBeTruthy();
  fireEvent.click(screen.getByRole('button',{name:'Restart to update'}));
  await waitFor(()=>expect(mock.update).toHaveBeenCalledExactlyOnceWith({type:'install',expected_build:31}));
  expect(screen.queryByText('Waiting for idle')).toBeNull();
});

describe('phase content and button labels',()=>{
  // [phase, status text, buttons left to right, extra status fields]
  const cases:[UpdatePhase,string,string[],Partial<UpdateStatus>?][]=[
    ['checking','Checking',['Close'],{candidate:null,progress:null}],
    ['idle','Ready',['Close'],{candidate:null,progress:null}],
    ['available','Update available',['Cancel','Download update'],{progress:null}],
    ['downloading','Downloading',['Hide','Cancel download'],{progress:42}],
    ['verifying','Verifying',['Hide','Cancel download'],{progress:100}],
    ['ready','Ready to install',['Cancel','Restart to update']],
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
    expect(within(dialog()!).getByRole('status').textContent).toBe(text);
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
  fireEvent.click(screen.getByRole('button',{name:'Update to v0.1.1'}));
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
