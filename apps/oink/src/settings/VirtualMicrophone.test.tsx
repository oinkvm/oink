// @vitest-environment jsdom
import {afterEach,beforeAll,expect,it,vi} from 'vitest';
import {cleanup,fireEvent,render,screen,waitFor,within} from '@testing-library/react';
import {OinkProvider,toast} from 'oink-ui';
import {api,shell} from '../api/client';
import {agent} from '../api/store';
import {setLanguage,tr} from '../i18n';
import type {MicrophonePluginStatus} from '../api/types';
import {VirtualMicrophone,VirtualMicrophoneDescription,jumpProgress,operationProgress,stepCaption} from './VirtualMicrophone';
beforeAll(()=>Object.defineProperty(window,'matchMedia',{value:()=>({matches:true,addEventListener(){},removeEventListener(){}})}));
afterEach(()=>{cleanup();agent.reset();vi.restoreAllMocks();});
const status=(installed:boolean,managed:boolean)=>({state:installed?'installed':'not_installed',operation:'idle',managed,input_loaded:installed,feed_loaded:installed,error_kind:null,last_operation_succeeded:null,last_operation:null} as MicrophonePluginStatus);
it.each([[false,false],[false,true],[true,false],[true,true]])('only explicitly acts when windows=%s installed=%s',async (windows,installed)=>{
 setLanguage('en');
 vi.spyOn(api,'microphonePluginStatus').mockResolvedValue(status(installed,true));
 const action=vi.spyOn(api,'microphonePluginAction').mockResolvedValue(status(!installed,true));
 render(<OinkProvider><VirtualMicrophoneDescription windows={windows}/><VirtualMicrophone windows={windows} active/></OinkProvider>);
 const button=await screen.findByRole('button',{name:installed?'Uninstall':'Install'});
 await waitFor(()=>expect(button.hasAttribute('disabled')).toBe(false));
 expect(action).not.toHaveBeenCalled();
 fireEvent.click(button);
 await waitFor(()=>expect(action).toHaveBeenCalledWith(installed?'uninstall':'install'));
});
it('keeps an unmanaged Windows copy disabled and links vendor terms',async()=>{
 setLanguage('en');
 vi.spyOn(api,'microphonePluginStatus').mockResolvedValue(status(true,false));
 const action=vi.spyOn(api,'microphonePluginAction');
 const open=vi.spyOn(shell,'openUrl').mockResolvedValue(undefined);
 render(<OinkProvider><VirtualMicrophoneDescription windows/><VirtualMicrophone windows active/></OinkProvider>);
 const button=await screen.findByRole('button',{name:'Uninstall'});
 expect(button.hasAttribute('disabled')).toBe(true);
 fireEvent.click(button);
 expect(action).not.toHaveBeenCalled();
 fireEvent.click(screen.getByRole('link',{name:'official website'}));
 expect(open).toHaveBeenCalledWith('https://www.vb-cable.com');
});

it.each(['zh-Hans','en','ja','ko'] as const)('keeps vendor licensing in one secondary sentence in %s', language=>{
 setLanguage(language);
 render(<VirtualMicrophoneDescription windows/>);
 const link=screen.getByRole('link');
 expect(link.getAttribute('href')).toBe('https://www.vb-cable.com');
 expect(link.parentElement?.className).toBe('setp__microphone-description');
 expect(screen.queryByRole('button')).toBeNull();
 if(language==='zh-Hans')expect(link.parentElement?.textContent).toBe('使用 VB-CABLE：访问官网查看许可证条款和捐赠');
});
it.each(['unsupported','not_installed'] as const)('respects supported installation state %s',async state=>{
 setLanguage('en');
 vi.spyOn(api,'microphonePluginStatus').mockResolvedValue({...status(false,false),state});
 render(<OinkProvider><VirtualMicrophone windows active/></OinkProvider>);
 await waitFor(()=>expect(screen.getByRole('button',{name:'Install'}).hasAttribute('disabled')).toBe(state==='unsupported'));
});

it.each(['en','zh-Hans','ja','ko'] as const)('explains the unmanaged VB-CABLE installation in %s',async language=>{
 setLanguage(language);
 vi.spyOn(api,'microphonePluginStatus').mockResolvedValue(status(true,false));
 render(<OinkProvider><VirtualMicrophone windows active/></OinkProvider>);
 const button=await screen.findByRole('button');
 await waitFor(()=>expect(button.hasAttribute('disabled')).toBe(true));
 // The wrapper receives hover because the uninstall button is intentionally disabled.
 fireEvent.pointerEnter(button.parentElement!,{pointerType:'mouse'});
 const copy={en:'VB-CABLE is installed but is not managed by Oink','zh-Hans':'VB-CABLE 已安装，但不由 Oink 管理',ja:'VB-CABLE はインストールされていますが、Oink では管理されていません',ko:'VB-CABLE이 설치되어 있지만 Oink에서 관리하지 않습니다'};
 await waitFor(()=>expect(screen.getByRole('tooltip').textContent).toBe(copy[language]));
});

it.each(['install','uninstall'] as const)('toasts immediate %s failures once without an inline status',async action=>{
 setLanguage('en');
 const danger=vi.spyOn(toast,'danger');
 vi.spyOn(api,'microphonePluginStatus').mockResolvedValue(status(action==='uninstall',true));
 vi.spyOn(api,'microphonePluginAction').mockResolvedValue({...status(false,true),last_operation_succeeded:false,error_kind:'other'});
 render(<OinkProvider><VirtualMicrophone windows active/></OinkProvider>);
 const button=await screen.findByRole('button',{name:action==='install'?'Install':'Uninstall'});
 await waitFor(()=>expect(button.hasAttribute('disabled')).toBe(false));
 fireEvent.click(button);
 await waitFor(()=>expect(danger).toHaveBeenCalledTimes(1));
 expect(danger).toHaveBeenCalledWith({title:tr('虚拟麦克风操作失败，请重试')});
 expect(document.querySelector('.setp__btns p[role="status"]')).toBeNull();
});
it.each(['cancelled','other'] as const)('toasts polled %s completion once',async error_kind=>{
 setLanguage('en');
 const notify=vi.spyOn(toast,error_kind==='cancelled'?'info':'danger');
 const query=vi.spyOn(api,'microphonePluginStatus').mockResolvedValue(status(false,true));
 vi.spyOn(api,'microphonePluginAction').mockResolvedValue({...status(false,true),operation:'installing'});
 render(<OinkProvider><VirtualMicrophone windows active/></OinkProvider>);
 const button=await screen.findByRole('button',{name:'Install'});
 await waitFor(()=>expect(button.hasAttribute('disabled')).toBe(false));
 query.mockResolvedValue({...status(false,true),last_operation_succeeded:false,error_kind});
 fireEvent.click(button);
 await waitFor(()=>expect(notify).toHaveBeenCalledTimes(1),{timeout:2000});
 expect(notify).toHaveBeenCalledWith({title:tr(error_kind==='cancelled'?'已取消':'虚拟麦克风操作失败，请重试')});
 expect(document.querySelector('.setp__btns p[role="status"]')).toBeNull();
});
it('does not replay a previous failed operation when opening settings',async()=>{
 const danger=vi.spyOn(toast,'danger');
 vi.spyOn(api,'microphonePluginStatus').mockResolvedValue({...status(false,true),last_operation_succeeded:false,error_kind:'other'});
 render(<OinkProvider><VirtualMicrophone windows active/></OinkProvider>);
 await waitFor(()=>expect(screen.getByRole('button').hasAttribute('disabled')).toBe(false));
 expect(danger).not.toHaveBeenCalled();
 expect(document.querySelector('.setp__btns p[role="status"]')).toBeNull();
});
it('toasts transport rejection',async()=>{
 const danger=vi.spyOn(toast,'danger');
 vi.spyOn(api,'microphonePluginStatus').mockResolvedValue(status(false,true));
 vi.spyOn(api,'microphonePluginAction').mockRejectedValue(new Error('offline'));
 render(<OinkProvider><VirtualMicrophone windows active/></OinkProvider>);
 const button=screen.getByRole('button');
 await waitFor(()=>expect(button.hasAttribute('disabled')).toBe(false));
 fireEvent.click(button);
 await waitFor(()=>expect(danger).toHaveBeenCalledTimes(1));
});

it('renders a prewarmed cached status immediately while a refresh is pending',()=>{
 setLanguage('en');
 agent.adopt('microphonePlugin',{...status(false,true),refreshing:true});
 vi.spyOn(api,'microphonePluginStatus').mockReturnValue(new Promise(()=>{}));
 render(<OinkProvider><VirtualMicrophone windows active/></OinkProvider>);
 expect(screen.getByRole('button',{name:'Install'}).hasAttribute('disabled')).toBe(false);
});
it('retains cached state when settings is reopened before its query returns',()=>{
 setLanguage('en');
 agent.adopt('microphonePlugin',status(true,true));
 vi.spyOn(api,'microphonePluginStatus').mockReturnValue(new Promise(()=>{}));
 const view=render(<OinkProvider><VirtualMicrophone windows active={false}/></OinkProvider>);
 view.rerender(<OinkProvider><VirtualMicrophone windows active/></OinkProvider>);
 expect(screen.getByRole('button',{name:'Uninstall'}).hasAttribute('disabled')).toBe(false);
});

it('keeps the specific failure detail and emits the operation result only once',async()=>{
 setLanguage('en');
 const danger=vi.spyOn(toast,'danger');
 const query=vi.spyOn(api,'microphonePluginStatus').mockResolvedValue(status(false,true));
 vi.spyOn(api,'microphonePluginAction').mockResolvedValue({...status(false,true),operation:'installing',step:'install_driver'});
 render(<OinkProvider><VirtualMicrophone windows active/></OinkProvider>);
 await waitFor(()=>expect(screen.getByRole('button').hasAttribute('disabled')).toBe(false));
 query.mockResolvedValue({...status(false,true),last_operation_succeeded:false,error_kind:'other',detail:'VB-CABLE install failed: 5'});
 fireEvent.click(screen.getByRole('button'));
 await waitFor(()=>expect(danger).toHaveBeenCalledTimes(1),{timeout:2000});
 expect(danger).toHaveBeenCalledWith({title:tr('虚拟麦克风操作失败，请重试'),description:'VB-CABLE install failed: 5'});
 await new Promise(resolve=>setTimeout(resolve,850));
 expect(danger).toHaveBeenCalledTimes(1);
});

it('does not toast again when a failed poll is followed by the failed operation result',async()=>{
 const danger=vi.spyOn(toast,'danger');
 const query=vi.spyOn(api,'microphonePluginStatus').mockResolvedValue(status(false,true));
 vi.spyOn(api,'microphonePluginAction').mockResolvedValue({...status(false,true),operation:'installing'});
 render(<OinkProvider><VirtualMicrophone windows active/></OinkProvider>);
 await waitFor(()=>expect(screen.getByRole('button').hasAttribute('disabled')).toBe(false));
 query.mockRejectedValue(new Error('offline'));
 fireEvent.click(screen.getByRole('button'));
 await waitFor(()=>expect(danger).toHaveBeenCalledTimes(1),{timeout:2000});
 query.mockResolvedValue({...status(false,true),last_operation_succeeded:false,error_kind:'other'});
 await waitFor(()=>expect(screen.getByRole('button').hasAttribute('disabled')).toBe(false),{timeout:2000});
 expect(danger).toHaveBeenCalledTimes(1);
});

it('holds simulated progress below each step node until the backend moves on',()=>{
 expect(operationProgress(true,'install','verify',0)).toBe(0);
 expect(operationProgress(true,'install','extract',0)).toBe(10);
 expect(operationProgress(true,'install','install_driver',8000)).toBeCloseTo(25+60*0.9*(1-Math.exp(-1)));
 expect(operationProgress(true,'install','install_driver',1e9)).toBeCloseTo(79);
 expect(operationProgress(true,'install','install_driver',1e9)).toBeLessThan(85);
 expect(operationProgress(true,'uninstall','uninstall_driver',1e9)).toBeCloseTo(77.5);
 expect(operationProgress(false,'install','copy_plugin',1e9)).toBeCloseTo(32.5);
 expect(operationProgress(false,'uninstall','remove_plugin',1e9)).toBeCloseTo(36);
 expect(operationProgress(false,'uninstall','finish',-500)).toBe(85);
 expect(operationProgress(true,'install',null,5000)).toBe(0);
 expect(operationProgress(true,'install','copy_plugin',5000)).toBe(0);
});
it('jumps to the new node with an ease-out and never moves backwards',()=>{
 expect(jumpProgress(25,null,0,200)).toBe(25);
 expect(jumpProgress(25,19,0,200)).toBe(19);
 expect(jumpProgress(25,19,100,200)).toBeCloseTo(19+6*0.875);
 expect(jumpProgress(25,19,200,200)).toBe(25);
 expect(jumpProgress(10,19,50,200)).toBe(19);
 expect(jumpProgress(100,79,0,0)).toBe(100);
});
it.each([
 ['zh-Hans',['正在验证安装包','正在解压','正在安装驱动','正在卸载驱动','正在复制插件','正在移除插件','正在重启音频服务','正在完成','正在验证']],
 ['en',['Verifying package','Extracting','Installing driver','Uninstalling driver','Copying plug-in','Removing plug-in','Restarting audio service','Finishing','Verifying']],
 ['ja',['インストールパッケージを確認中','展開中','ドライバーをインストール中','ドライバーをアンインストール中','プラグインをコピー中','プラグインを削除中','オーディオサービスを再起動中','完了処理中','確認中']],
 ['ko',['설치 패키지 확인 중','압축 해제 중','드라이버 설치 중','드라이버 제거 중','플러그인 복사 중','플러그인 제거 중','오디오 서비스 다시 시작 중','마무리 중','확인 중']],
] as const)('names each step in %s',(language,expected)=>{
 setLanguage(language);
 const steps=['verify','extract','install_driver','uninstall_driver','copy_plugin','remove_plugin','restart_audio','finish'] as const;
 expect([...steps.map(step=>stepCaption('install',step)),stepCaption('uninstall','verify')]).toEqual(expected);
 expect(stepCaption('install',null)).toBe('');
});
it('replaces the button with the step progress computed from the step start',async()=>{
 setLanguage('zh-Hans');
 const running={...status(false,true),operation:'installing',step:'install_driver',step_started_at_ms:Date.now()-8000} as MicrophonePluginStatus;
 agent.adopt('microphonePlugin',running);
 vi.spyOn(api,'microphonePluginStatus').mockResolvedValue(running);
 render(<OinkProvider><VirtualMicrophone windows active/></OinkProvider>);
 const bar=screen.getByRole('progressbar',{name:'正在安装驱动'});
 expect(Number(bar.getAttribute('aria-valuenow'))).toBeGreaterThanOrEqual(58);
 expect(Number(bar.getAttribute('aria-valuenow'))).toBeLessThan(62);
 expect(screen.getByText('正在安装驱动').className).toBe('setp__vmic-caption');
 expect(screen.queryByRole('button')).toBeNull();
});
it('finishes a successful operation at 100% before showing the installed row',async()=>{
 setLanguage('en');
 const query=vi.spyOn(api,'microphonePluginStatus').mockResolvedValue(status(false,true));
 vi.spyOn(api,'microphonePluginAction').mockResolvedValue({...status(false,true),operation:'installing',step:'verify',step_started_at_ms:Date.now()});
 render(<OinkProvider><VirtualMicrophone windows={false} active/></OinkProvider>);
 const button=await screen.findByRole('button',{name:'Install'});
 await waitFor(()=>expect(button.hasAttribute('disabled')).toBe(false));
 fireEvent.click(button);
 expect(await screen.findByRole('progressbar',{name:'Verifying package'})).toBeTruthy();
 query.mockResolvedValue({...status(true,true),last_operation_succeeded:true});
 await waitFor(()=>expect(screen.getByRole('progressbar').getAttribute('aria-valuenow')).toBe('100'),{timeout:2000});
 expect(await screen.findByRole('button',{name:'Uninstall'})).toBeTruthy();
});
it.each([['install','Restart to finish installing','Restart the computer to finish installing the virtual microphone. Save your work first.'],['uninstall','Restart to finish uninstalling','Restart the computer to finish uninstalling the virtual microphone. Save your work first.']] as const)('asks before restarting Windows after %s',async(operation,caption,body)=>{
 setLanguage('en');
 // A leftover device after uninstalling reads as unmanaged; the copy follows last_operation, not ownership.
 const pendingRestart={...status(true,false),state:'unmanaged',restart_required:true,last_operation:operation} as MicrophonePluginStatus;
 vi.spyOn(api,'microphonePluginStatus').mockResolvedValue(pendingRestart);
 const restart=vi.spyOn(api,'restartSystem').mockResolvedValue(null);
 render(<OinkProvider><VirtualMicrophone windows active/></OinkProvider>);
 fireEvent.click(await screen.findByRole('button',{name:'Restart'}));
 expect(screen.getByText(caption).className).toBe('setp__vmic-caption');
 let dialog=await screen.findByRole('dialog',{name:'Restart computer'});
 expect(dialog.textContent).toContain(body);
 fireEvent.click(within(dialog).getByRole('button',{name:'Cancel'}));
 await waitFor(()=>expect(screen.queryByRole('dialog')).toBeNull());
 expect(restart).not.toHaveBeenCalled();
 fireEvent.click(screen.getByRole('button',{name:'Restart'}));
 dialog=await screen.findByRole('dialog',{name:'Restart computer'});
 fireEvent.click(within(dialog).getByRole('button',{name:'Restart'}));
 await waitFor(()=>expect(restart).toHaveBeenCalledTimes(1));
});
it('never offers a system restart on macOS',async()=>{
 setLanguage('en');
 vi.spyOn(api,'microphonePluginStatus').mockResolvedValue({...status(true,true),restart_required:true});
 render(<OinkProvider><VirtualMicrophone windows={false} active/></OinkProvider>);
 expect(await screen.findByRole('button',{name:'Uninstall'})).toBeTruthy();
 expect(screen.queryByRole('button',{name:'Restart'})).toBeNull();
});
it('explains the explicit unmanaged state',async()=>{
 setLanguage('zh-Hans');
 vi.spyOn(api,'microphonePluginStatus').mockResolvedValue({...status(true,false),state:'unmanaged'});
 render(<OinkProvider><VirtualMicrophone windows active/></OinkProvider>);
 const button=await screen.findByRole('button',{name:'卸载'});
 await waitFor(()=>expect(button.hasAttribute('disabled')).toBe(true));
 fireEvent.pointerEnter(button.parentElement!,{pointerType:'mouse'});
 await waitFor(()=>expect(screen.getByRole('tooltip').textContent).toBe('VB-CABLE 已安装，但不由 Oink 管理'));
});
