import { useEffect, useRef, useState } from 'react';
import { api, ApiFailure } from '../api/client';
import { agent, useAgent } from '../api/store';
import { pairingApi, pairingCursor, savePairingCursor } from '../api/pairing';
import type { PairView, PairingEvent } from '../api/pairing';
import { DEFAULT_PORT } from './data';
import type { IncomingRequest, Peer } from './data';
import { problemText, tr } from '../i18n';
import { speed, toast } from 'oink-ui';
import { fingerprint } from '../api/identity';
import { PairFailure, AddressQueryOwner, addressText, presentPeer, pairProblemText } from './model';
export type PairStatus = 'connecting' | 'input' | 'verifying' | 'wrong' | 'success' | 'failed';
export interface PairSession {
  key: number; pairId: string; peer: Peer; dir:'in'|'out'; code:string; input:string;
  status:PairStatus; reason?:string; source:'request'|'nearby'|'manual'; wrongs:number;
  sourceCount:number|null; targetCount:number|null;
}
export interface PairDialogState {open:boolean;mode:'manual'|'pair';session:PairSession|null;key:number}
const stableDevice = (p:Peer) => p.device;
const errorText = (error:unknown) => error instanceof PairFailure ? pairProblemText(error.problem) : error instanceof ApiFailure ? problemText(error.problem.code) : error instanceof Error ? error.message : problemText('unavailable');
async function editBlock(device:string,blocked:boolean) {
  const result=await (blocked?api.block:api.unblock)(device);
  if(!('snapshot' in result))throw new Error(({0:tr('设备身份无效'),1:tr('无法保存屏蔽名单'),2:tr('屏蔽名单已达上限'),3:problemText('unavailable')})[result.code]??tr('无法更新屏蔽名单'));
  agent.receive({type:'blocked_devices',data:{snapshot:result.snapshot}});
}
export function usePairing(activeWindow = true) {
  const state=useAgent();
  const localDevice=state.overview?.this_device.id;
  const [dialog,setDialog]=useState<PairDialogState>({open:false,mode:'manual',session:null,key:0});
  const [input,setInput]=useState('');
  const [codes,setCodes]=useState<Record<string,string>>({});
  const [submitting,setSubmitting]=useState(false);
  const [terminals,setTerminals]=useState<Record<string,{id:string;status:'success'|'failed';reason?:string}>>({});
  const setTerminal=(value:{id:string;status:'success'|'failed';reason?:string})=>setTerminals(previous=>({...previous,[value.id]:value}));
  const terminal=dialog.session?terminals[dialog.session.pairId]:undefined;
  const manual=useRef(new AddressQueryOwner());
  const active=useRef(dialog);active.current=dialog;
  const codeRequests=useRef(new Set<string>());
  const dismissed=useRef(new Set<string>());
  const refreshPending=useRef(new Map<string,{resolve:()=>void;reject:(error:Error)=>void}>());
  const run=async<T,>(operation:()=>Promise<T>):Promise<T>=>{try{return await operation();}catch(error){if(!(error instanceof PairFailure))agent.fail(error instanceof ApiFailure?error.problem.code:'unavailable');throw error;}};
  const openPair=(id:string,peer:Peer,source:PairSession['source'],counts?:{sourceCount:number|null;targetCount:number|null})=>{
    setInput('');setSubmitting(false);
    setDialog(d=>({open:true,mode:'pair',key:d.key+1,session:{key:d.key+1,pairId:id,peer,dir:source==='request'?'in':'out',code:'',input:'',status:'connecting',source,wrongs:0,sourceCount:counts?.sourceCount??null,targetCount:counts?.targetCount??null}}));
  };
  useEffect(()=>{
    const stop=agent.onEvent(event=>{
      manual.current.receive(event as PairingEvent);
      if(event.type==='membership_edited' && activeWindow && event.data.result.type==='problem')toast({title:tr('无法撤销配对'),description:pairProblemText(event.data.result.problem)});
      if(event.type==='nearby_refresh_finished') {
        const pending=refreshPending.current.get(event.data.operation_id);if(!pending)return;
        refreshPending.current.delete(event.data.operation_id);
        if(event.data.result.type==='ok')pending.resolve();else pending.reject(new Error(pairProblemText(event.data.result.problem)));
      }
      if(event.type==='pairing_changed' && event.data.view.direction==='incoming' && event.data.view.phase===2 && activeWindow && !active.current.open && !dismissed.current.has(event.data.view.id) && event.data.view.peer.verified) {
        openPair(event.data.view.id,presentPeer(event.data.view.peer.verified),'request');
      }
    });
    return()=>{void manual.current.close().catch(()=>{});stop();};
  },[]);
  const restored=useRef(false);
  useEffect(()=>{if(!activeWindow||!state.boot||!localDevice)return;let active=true;void pairingApi.subscribe(pairingCursor(localDevice)).then(events=>{if(active)events.forEach(event=>agent.receive(event));}).catch(()=>agent.fail('unavailable'));return()=>{active=false;};},[activeWindow,state.boot,localDevice]);
  useEffect(()=>{
    if(!activeWindow||!state.pairings||restored.current)return;restored.current=true;
    const incoming=state.pairings.sessions.find(p=>p.direction==='incoming'&&p.phase===2&&p.peer.verified);
    if(incoming?.peer.verified)openPair(incoming.id,presentPeer(incoming.peer.verified),'request');
  },[activeWindow,state.pairings]);
  const current=state.pairings?.sessions.find(p=>p.id===dialog.session?.pairId);
  useEffect(()=>{
    if(!dialog.open||!current||current.phase!==1||codes[current.id]||codeRequests.current.has(current.id))return;
    const id=current.id;codeRequests.current.add(id);
    void pairingApi.code(id).then(code=>{if(active.current.open&&active.current.session?.pairId===id)setCodes(previous=>({...previous,[id]:code.six_digits}));}).catch(error=>setTerminal({id,status:'failed',reason:errorText(error)})).finally(()=>codeRequests.current.delete(id));
  },[dialog.open,current?.id,current?.phase,codes]);
  useEffect(()=>{
    if(!activeWindow||!localDevice)return;
    for(const event of [...state.pairingEvents].sort((a,b)=>BigInt(a.data.event_id)<BigInt(b.data.event_id)?-1:1)) {
      const id=event.data.event_id;
      if(BigInt(id)<=BigInt(pairingCursor(localDevice))) { void pairingApi.ack(id).then(()=>agent.consumePairingEvent(id)).catch(()=>agent.fail('unavailable'));continue; }
      try {savePairingCursor(localDevice,id);}catch {agent.fail('persistence_failed');return;}
      if(event.type==='pairing_completed') {
        setTerminal({id:event.data.pair_id,status:'success'});
        toast({title:tr('已与 {0} 配对', event.data.peer.name),action:{label:tr('撤销'),onClick:()=>{void run(()=>pairingApi.undo(crypto.randomUUID(),event.data.undo)).catch(error=>toast({title:tr('无法撤销配对'),description:errorText(error)}));}}});
      } else {
        const reason=pairProblemText(event.data.problem);setTerminal({id:event.data.pair_id,status:'failed',reason});
        if(active.current.session?.pairId!==event.data.pair_id)toast({title:tr('配对失败'),description:reason});
      }
      void pairingApi.ack(id).then(()=>agent.consumePairingEvent(id)).catch(()=>agent.fail('unavailable'));
    }
  },[state.pairingEvents,activeWindow,localDevice]);
  const blocked:Peer[]=state.blocked?.entries.map(b=>({compatible:null,id:b.device,device:b.device,deviceId:fingerprint(b.device),name:b.name??fingerprint(b.device),os:b.os==='macos'?'mac':b.os==='windows'?'win':b.os==='linux'?'linux':'other',ip:'',port:DEFAULT_PORT}))??[];
  const members=new Set(state.cards?.cards.map(card=>card.device));
  const nearby:Peer[]=state.nearby?.rows.filter(peer=>!members.has(peer.device)).map(p=>({compatible:p.compatible,problem:p.problem?pairProblemText(p.problem):p.compatible?undefined:tr('协议版本不兼容'),id:p.device,device:p.device,name:p.name,os:p.os==='macos'?'mac':p.os==='windows'?'win':p.os==='linux'?'linux':'other',deviceId:fingerprint(p.device),ip:addressText(p.selected),port:p.selected.port}))??[];
  const requests:IncomingRequest[]=state.pairings?.requests.map(p=>({...presentPeer(p.peer),id:p.id,sourceCount:p.source_count??null,targetCount:p.target_count,revision:p.revision}))??[];
  // Returning from onboarding hides the code entry without rejecting the request.
  // The live accepted session remains resumable from the regular sidebar.
  for (const session of state.pairings?.sessions ?? []) {
    if (session.direction === 'incoming' && session.phase === 2 && session.peer.verified && !requests.some(r => r.id === session.id))
      requests.push({ ...presentPeer(session.peer.verified), id: session.id, revision: session.revision, sourceCount: null, targetCount: null });
  }
  const start=async(peer:Peer,target:Parameters<typeof pairingApi.start>[1],source:PairSession['source'])=>{
    let reply:Awaited<ReturnType<typeof pairingApi.start>>;
    try{reply=await run(()=>pairingApi.start(crypto.randomUUID(),target));}catch(error){toast({title:tr('无法开始配对'),description:errorText(error)});throw error;}
    if(target.type==='prepared_query')manual.current.consume(target.query_id);
    openPair(reply.pair_id,peer,source);
    await run(()=>agent.query('pairings',pairingApi.pairings));
  };
  const actions={
    openManual:()=>{void manual.current.close().catch(()=>agent.fail('unavailable'));setDialog(d=>({open:true,mode:'manual',session:null,key:d.key+1}));},
    refresh:async()=>{
      const operation=crypto.randomUUID();
      return run(()=>new Promise<void>((resolve,reject)=>{
        refreshPending.current.set(operation,{resolve,reject});
        void pairingApi.refresh(operation).catch(error=>{refreshPending.current.delete(operation);reject(error);});
      }));
    },
    connect:async(p:Peer)=>{
      if(!p.compatible){toast({title:p.problem??tr('协议版本不兼容')});return;}
      const existing=agent.get().pairings?.sessions.find(s=>(s.peer.verified?.device??s.peer.expected_id)===p.device && s.phase<7);
      if(existing){openPair(existing.id,p,'nearby');return;}
      await start(p,{type:'nearby_device',device:p.device},'nearby');
    },
    accept:(req:IncomingRequest)=>{dismissed.current.delete(req.id);void run(async()=>{openPair(req.id,req,'request',req);if(!agent.get().pairings?.sessions.some(s=>s.id===req.id&&s.phase===2))await pairingApi.accept(req.id,req.revision);await agent.query('pairings',pairingApi.pairings);}).catch(error=>setTerminal({id:req.id,status:'failed',reason:errorText(error)}));},
    reject:async(req:IncomingRequest)=>{try{await run(async()=>{if(agent.get().pairings?.sessions.some(s=>s.id===req.id&&s.phase===2))await pairingApi.cancel(req.id,req.revision);else await pairingApi.decline(req.id,req.revision);});}catch(error){toast({title:tr('无法拒绝请求'),description:errorText(error)});}},
    block:async(p:Peer)=>{await run(()=>editBlock(stableDevice(p),true));toast({title:tr('已屏蔽 {0}', p.name),description:tr('它的配对请求会被自动拒绝，也不会出现在附近设备中'),action:{label:tr('撤销'),onClick:()=>{void run(()=>editBlock(stableDevice(p),false)).catch(()=>agent.fail('persistence_failed'));}}});},
    unblock:async(p:Peer)=>{await run(()=>editBlock(stableDevice(p),false));},
    search:async(ip:string,port:number)=>presentPeer((await manual.current.search(ip,port)).peer),
    pairFound:async(p:Peer)=>{let found;try{found=manual.current.get(p.device);}catch(error){toast({title:tr('无法开始配对'),description:errorText(error)});throw error;}await start(p,{type:'prepared_query',query_id:found.query_id,generation:found.generation},'manual');},
    setInput,
    submit:async(code:string)=>{
      const session=agent.get().pairings?.sessions.find(p=>p.id===dialog.session?.pairId);if(!session||session.phase!==2||submitting)return;
      setSubmitting(true);
      try {await run(()=>pairingApi.submit(session.id,session.revision,4-session.attempts_left,code));}
      catch(error){setTerminal({id:session.id,status:'failed',reason:errorText(error)});setSubmitting(false);}
    },
    cancel:async()=>{
      if(dialog.mode==='manual'){await manual.current.close();setDialog(d=>({...d,open:false}));return;}
      let session=agent.get().pairings?.sessions.find(p=>p.id===dialog.session?.pairId);
      if(!session&&!terminal){await run(()=>agent.query('pairings',pairingApi.pairings));session=agent.get().pairings?.sessions.find(p=>p.id===dialog.session?.pairId);if(!session)throw new Error(problemText('unavailable'));}
      if(session&&session.phase<7&&!terminal){const result=await run(()=>pairingApi.cancel(session.id,session.revision));if(result.type==='already_committed')toast({title:tr('配对已经提交'),action:{label:tr('撤销'),onClick:()=>{void run(()=>pairingApi.undo(crypto.randomUUID(),result.undo)).catch(error=>toast({title:tr('无法撤销配对'),description:errorText(error)}));}}});else if(result.type==='commit_unconfirmed')agent.fail('outcome_unknown');}
      setDialog(d=>({...d,open:false}));setInput('');setCodes({});
    },
    dismiss:()=>{if(dialog.session)dismissed.current.add(dialog.session.pairId);setDialog(d=>({...d,open:false,session:null}));setInput('');},
    exited:()=>setDialog(d=>d.open?d:{...d,session:null}),
  };
  useEffect(()=>{if(current && current.phase!==3)setSubmitting(false);},[current?.revision,current?.phase]);
  // Completed is authoritative even when its replay notification was acknowledged.
  useEffect(()=>{if(terminal?.status!=='success'&&current?.phase!==7)return;const id=dialog.session?.pairId;const timer=setTimeout(()=>setDialog(d=>d.session?.pairId===id?{...d,open:false}:d),600*Math.max(speed(),0.1));return()=>clearTimeout(timer);},[terminal?.id,terminal?.status,current?.phase,dialog.session?.pairId]);
  let session=dialog.session;
  if(session) {
    const view:PairView|undefined=current;
    const verified=view?.peer.verified;
    const status:PairStatus=terminal?.id===session.pairId?terminal.status:!view||view.phase===0||!verified?'connecting':view.phase===7?'success':view.phase>=8?'failed':view.phase>=3||view.feedback==='correct'?'verifying':view.feedback==='wrong'?'wrong':view.phase===1&&!codes[view.id]?'connecting':'input';
    session={...session,peer:verified?presentPeer(verified):session.peer,dir:view?.direction==='incoming'?'in':session.dir,code:codes[session.pairId]??'',input,status:submitting&&status!=='success'&&status!=='failed'?'verifying':status,wrongs:view?3-view.attempts_left:0,reason:terminal?.id===session.pairId?terminal.reason:view?.terminal?pairProblemText(view.terminal):undefined};
  }
  return {requests,nearby,blocked,dialog:{...dialog,session},actions,defaultPort:DEFAULT_PORT};
}
export type Pairing=ReturnType<typeof usePairing>;
