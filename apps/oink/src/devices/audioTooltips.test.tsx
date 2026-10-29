// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeAll, expect, it, vi } from 'vitest';
import { OinkProvider } from 'oink-ui';
import { DeviceItem, audioTooltip } from './DeviceList';
import { setLanguage } from '../i18n';
import type { Device } from './types';

const base:Device={id:'peer',name:'Peer',os:'win',group:'online',ip:'',port:0,latency:1,lastSeen:null,cursor:'remote',screen:'shared',mouse:'shared',keyboard:'shared',clipboard:'shared',gameMode:null,speaker:{mode:'unused',volume:70,muted:false}};
const source={captureActive:true,consumerActive:true,receivingActive:true,mode:'source',volume:100,muted:false,permissionGranted:true,virtualReady:true,canSetVolume:true} as const;
const copy={
  receiveOther:['Receive a microphone shared by another device','接收其他设备共享的麦克风','他のデバイスが共有するマイクを受信','다른 기기가 공유하는 마이크 수신'],
  shareThis:['Share this device’s microphone','共享本机麦克风','このデバイスのマイクを共有','이 기기의 마이크 공유'],
  stopSend:['Stop sharing','停止共享','共有を停止','공유 중지'],
  stopReceive:['Stop receiving','停止接收','受信を停止','수신 중지'],
  switchSend:['Switch to sending','改为发送','送信に切り替え','송신으로 전환'],
  send:['Microphone: Sending','麦克风：发送','マイク：送信','마이크: 송신'],
  receiving:['Microphone: Receiving','麦克风：接收','マイク：受信','마이크: 수신'],
  micLocal:['Microphone: Local','麦克风：本地','マイク：ローカル','마이크: 로컬'],
  local:['Speaker: Local','扬声器：本地','スピーカー：ローカル','스피커: 로컬'],
  forward:['Speaker: Forwarding to Studio','扬声器：转发到 Studio','スピーカー：Studio に転送','스피커: Studio에 전달'],
  noExit:['Speaker: Unavailable (no group output)','扬声器：不可用，无出口','スピーカー：利用不可（出力先なし）','스피커: 사용 불가(출력 기기 없음)'],
  exit:['Speaker: Audio exit','扬声器：出口','スピーカー：出力先','스피커: 출력 기기'],
  unknown:['Speaker: Unavailable (Status unknown)','扬声器：不可用，状态未知','スピーカー：利用不可（状態は不明）','스피커: 사용 불가(상태 알 수 없음)'],
  source:['Sharing microphone','共享麦克风','マイクを共有中','마이크 공유 중'],
  sourceMuted:['Sharing microphone, muted','共享麦克风，已静音','マイクを共有中（消音）','마이크 공유 중, 음소거됨'],
  receiver:['Receiving microphone','接收麦克风','マイクを受信中','마이크 수신 중'],
  muted:['Microphone muted','麦克风已静音','マイクは消音中','마이크 음소거됨'],
  idle:['Waiting for a shared microphone','等待共享麦克风','共有マイクを待機中','공유 마이크 대기 중'],
  unavailable:['Unavailable; install the virtual microphone','不可用，请安装虚拟麦克风','使用できません。仮想マイクをインストールしてください','사용할 수 없습니다. 가상 마이크를 설치하세요'],
  toLocal:['Stop sharing','停止共享','共有を停止','공유 중지'],
  toForward:['Send audio','发送音频','音声を送信','오디오 보내기'],
  toExit:['Set as audio output','设为音频出口','音声出力先に設定','오디오 출력으로 설정'],
  mute:['Mute','静音','消音','음소거'],
  unmute:['Unmute','取消静音','消音を解除','음소거 해제'],
  share:['Share microphone','共享此麦克风','マイクを共有','마이크 공유'],
  receive:['Receive and unmute','接收并取消静音','受信して消音解除','수신 및 음소거 해제'],
  settings:['Open Audio settings','前往音频设置','音声設定を開く','오디오 설정 열기'],
  select:['Select device','选中设备','デバイスを選択','기기 선택'],
};
type Key=keyof typeof copy;
const cases:{name:string;kind:'speaker'|'microphone';device:Device;exit:string|null;state:Key;click:Key;double:Key|null}[]=[];
// The first line names only the state: mute, activity and waiting details stay out of it.
for(const muted of [false,true]){
  cases.push({name:'speaker local '+muted,kind:'speaker',device:{...base,speaker:{...base.speaker,localOutput:{volume:50,muted}}},exit:'Studio',state:'local',click:'toForward',double:'toExit'});
  cases.push({name:'speaker exit '+muted,kind:'speaker',device:{...base,speaker:{...base.speaker,mode:'exit',muted}},exit:'Peer',state:'exit',click:muted?'unmute':'mute',double:'toLocal'});
  for(const exit of ['Studio',null])cases.push({name:'speaker forwarding '+muted+' '+exit,kind:'speaker',device:{...base,speaker:{...base.speaker,mode:'forwarding',muted}},exit,state:exit?'forward':'noExit',click:muted?'unmute':'mute',double:'toLocal'});
  cases.push({name:'microphone source '+muted,kind:'microphone',device:{...base,microphone:{...source,muted}},exit:null,state:'send',click:muted?'unmute':'mute',double:'stopSend'});
}
cases.push({name:'speaker forwarding paused',kind:'speaker',device:{...base,speaker:{...base.speaker,mode:'forwarding',activity:'paused'}},exit:'Studio',state:'forward',click:'mute',double:'toLocal'});
cases.push({name:'microphone source waiting for a receiver',kind:'microphone',device:{...base,microphone:{...source,captureActive:false}},exit:null,state:'send',click:'mute',double:'stopSend'});
for(const mode of ['unknown','fault'] as const)cases.push({name:'speaker '+mode,kind:'speaker',device:{...base,speaker:{...base.speaker,mode}},exit:null,state:'unknown',click:'select',double:null});
// Permission and setup hints use PermissionHint, covered in permissionHints.test.tsx.
for(const mode of ['receiver','muted','idle'] as const)cases.push({name:'microphone '+mode,kind:'microphone',device:{...base,microphone:{...source,mode}},exit:null,state:mode==='muted'?'micLocal':'receiving',click:mode==='muted'?'receiveOther':'stopReceive',double:mode==='muted'?'shareThis':'switchSend'});

beforeAll(()=>Object.defineProperty(window,'matchMedia',{value:()=>({matches:false,addEventListener(){},removeEventListener(){}})}));
afterEach(()=>{cleanup();vi.useRealTimers();setLanguage('en');});
it.each((['en','zh-Hans','ja','ko'] as const).flatMap((language,index)=>cases.map(c=>({...c,language,index}))))('$language $name shows only the state and its available gestures',async c=>{
  vi.useFakeTimers();setLanguage(c.language);
  const {container}=render(<OinkProvider><DeviceItem d={c.device} exitName={c.exit} now={0} selected={false} collapsed={false} onSelect={()=>{}} onToggle={()=>{}} onSpeaker={()=>{}} onMicrophone={()=>{}}/></OinkProvider>);
  fireEvent.pointerEnter(container.querySelectorAll('.dev-ico')[c.kind==='speaker'?2:3],{pointerType:'mouse'});
  await act(async()=>{await vi.advanceTimersByTimeAsync(1000);});
  const lines=Array.from(screen.getByRole('tooltip').querySelectorAll('.dev-ico__tip > span')).map(el=>el.textContent);
  const prefix=['Click: ','单击：','クリック：','클릭: '][c.index];
  const double=['Double-click: ','双击：','ダブルクリック：','두 번 클릭: '][c.index];
  expect(lines).toEqual([copy[c.state][c.index],prefix+copy[c.click][c.index],...(c.double?[double+copy[c.double][c.index]]:[])]);
});

it.each((['keyboard','mouse'] as const).flatMap(kind=>(['shared','local','disabled'] as const).flatMap(mode=>[false,true].map(canDisableMouse=>({kind,mode,canDisableMouse})))))('$kind $mode (mouse disable=$canDisableMouse) shows the available gestures',async({kind,mode,canDisableMouse})=>{
  vi.useFakeTimers();setLanguage('zh-Hans');
  const {container}=render(<OinkProvider><DeviceItem d={{...base,[kind]:mode,canDisableMouse}} now={0} selected={false} collapsed={false} onSelect={()=>{}} onToggle={()=>{}}/></OinkProvider>);
  fireEvent.pointerEnter(container.querySelectorAll('.dev-ico')[kind==='mouse'?0:1],{pointerType:'mouse'});
  await act(async()=>{await vi.advanceTimersByTimeAsync(1000);});
  const state={shared:'共享',local:'本地',disabled:'禁用'}[mode];
  const action=mode==='disabled'?'恢复共享':mode==='shared'?'停止共享':'切换为共享';
  expect([...screen.getByRole('tooltip').querySelectorAll('.dev-ico__tip > span')].map(line=>line.textContent)).toEqual([
    `${kind==='keyboard'?'键盘':'鼠标'}：${state}`,`单击：${action}`,
    ...(mode!=='disabled'&&(kind==='keyboard'||canDisableMouse)?['双击：禁用']:[]),
  ]);
});

it.each((['en','ja','ko'] as const).flatMap((language,index)=>(['keyboard','mouse'] as const).flatMap(kind=>(['shared','local','disabled'] as const).map(mode=>({language,index,kind,mode})))))('$language $kind $mode translates each tooltip line',async({language,index,kind,mode})=>{
  vi.useFakeTimers();setLanguage(language);
  const {container}=render(<OinkProvider><DeviceItem d={{...base,[kind]:mode,canDisableMouse:true}} now={0} selected={false} collapsed={false} onSelect={()=>{}} onToggle={()=>{}}/></OinkProvider>);
  fireEvent.pointerEnter(container.querySelector(`[data-glyph="${kind}"]`)!,{pointerType:'mouse'});
  await act(async()=>{await vi.advanceTimersByTimeAsync(1000);});
  const name=(kind==='keyboard'?['Keyboard','キーボード','키보드']:['Mouse','マウス','마우스'])[index];
  const state=kind==='keyboard' && mode==='disabled' && language==='ko'?'비활성화됨':{shared:['shared','共有','공유'],local:['local','ローカル','로컬'],disabled:['disabled','無効','비활성화']}[mode][index];
  const single={shared:['Stop sharing','共有を停止','공유 중지'],local:['Switch to Shared','共有に切り替え','공유(으)로 전환'],disabled:['Resume sharing','共有を再開','공유 재개']}[mode][index];
  expect([...screen.getByRole('tooltip').querySelectorAll('.dev-ico__tip > span')].map(line=>line.textContent)).toEqual([
    name+[': ','：',': '][index]+state,['Click: ','クリック：','클릭: '][index]+single,
    ...(mode==='disabled'?[]:[['Double-click: Disable','ダブルクリック：無効にする','두 번 클릭: 사용 안 함'][index]]),
  ]);
});
it.each((['keyboard','mouse'] as const).flatMap(kind=>[
  {kind,patch:{inputPermissionMissing:true}},
  {kind,patch:{actionability:{[kind]:'unavailable'}}},
  {kind,patch:{actionability:{[kind]:'open_permissions'}}},
  {kind,patch:{[kind]:'fault'}},
  {kind,patch:{[kind]:'unknown'}},
]))('$kind gates suppress double-click actions and hints: $patch',async({kind,patch})=>{
  vi.useFakeTimers();setLanguage('zh-Hans');
  const toggle=vi.fn(),grant=vi.fn();
  const {container}=render(<OinkProvider><DeviceItem d={{...base,group:'self',os:'mac',canDisableMouse:true,...patch} as Device} now={0} selected={false} collapsed={false} onSelect={()=>{}} onToggle={toggle} onGrantPermission={grant}/></OinkProvider>);
  const icon=container.querySelector(`[data-glyph="${kind}"]`)!;
  fireEvent.pointerEnter(icon,{pointerType:'mouse'});
  await act(async()=>{await vi.advanceTimersByTimeAsync(1000);});
  expect(screen.getByRole('tooltip').textContent).not.toContain('双击');
  fireEvent.click(icon,{detail:2});fireEvent.keyDown(icon,{key:'Enter',shiftKey:true});
  expect(toggle).not.toHaveBeenCalled();expect(grant).not.toHaveBeenCalled();
});

it.each(['en','zh-Hans','ja','ko'] as const)('shows the receive warning and explains an absent source in %s',async language=>{
  vi.useFakeTimers();setLanguage(language);
  const action=vi.fn();
  const {container}=render(<OinkProvider><DeviceItem d={{...base,microphone:{...source,mode:'idle',waitingForSource:true}}} now={0} selected={false} collapsed={false} onSelect={()=>{}} onMicrophone={action}/></OinkProvider>);
  const icon=container.querySelector('[data-glyph="microphone"]')!;
  expect(icon.querySelector('[data-badge="warning"]')).not.toBeNull();
  expect(icon.getAttribute('data-tone')).toBe('good');
  fireEvent.pointerEnter(icon,{pointerType:'mouse'});
  await act(async()=>{await vi.advanceTimersByTimeAsync(1000);});
  const copy={en:'No device is currently sharing a microphone','zh-Hans':'当前没有设备共享麦克风',ja:'現在マイクを共有しているデバイスはありません',ko:'현재 마이크를 공유하는 기기가 없습니다'};
  expect(screen.getByRole('tooltip').textContent).toContain(copy[language]);
  fireEvent.click(icon,{detail:1});expect(action).toHaveBeenCalledWith('local');
});

it('gives the reason when the speaker or microphone is unavailable',()=>{
  setLanguage('zh-Hans');
  expect(audioTooltip({...base,speaker:{...base.speaker,mode:'fault'},faults:{speaker:'清理未完成'}},'speaker',null)[0]).toBe('扬声器：不可用，清理未完成');
  expect(audioTooltip({...base,speaker:{...base.speaker,mode:'forwarding',selectedExitUnavailable:true}},'speaker',null)[0]).toBe('扬声器：不可用，所选出口不可用');
  expect(audioTooltip({...base,microphone:{...source,fault:{code:'device_unavailable'}}} as Device,'microphone',null)[0]).toBe('麦克风：不可用，设备不可用');
  expect(audioTooltip({...base,microphone:{...source,permissionGranted:false}},'microphone',null)[0]).toBe('麦克风：不可用，系统权限被拒绝');
});


it('explains a microphone forwarding loopback selection in every locale',()=>{
  const copy = {
    'zh-Hans':'麦克风：不可用，请在来源设备选择硬件麦克风，不能共享虚拟回环输入',
    en:'Microphone: Unavailable (Select a hardware microphone on the source device; a virtual loopback input cannot be shared)',
    ja:'マイク：利用不可（送信元デバイスでハードウェアマイクを選択してください。仮想ループバック入力は共有できません）',
    ko:'마이크: 사용 불가(소스 기기에서 하드웨어 마이크를 선택하세요. 가상 루프백 입력은 공유할 수 없습니다)',
  };
  for (const locale of Object.keys(copy) as (keyof typeof copy)[]) {
    setLanguage(locale);
    expect(audioTooltip({...base,microphone:{...source,fault:{code:'microphone_loopback'}}} as Device,'microphone',null)[0]).toBe(copy[locale]);
  }
  setLanguage('zh-Hans');
});
