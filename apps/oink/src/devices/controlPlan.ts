import type { DeviceCardsView, DeviceCard, CardAction } from '../api/cards';
import type { ControlAction, ControlObservation } from './controlActions';
import { resourceAction } from './project';

export function controlCommand(card:DeviceCard,action:ControlAction):CardAction|null|undefined {
  if(action.kind==='card')return resourceAction(card,action.resource,action.desired);
  if(action.kind==='speaker')return resourceAction(card,'speaker',action.mode);
  if(action.kind==='microphone') {
    const mic=card.status?.microphone;
    if(action.mode==='source'&&!mic?.permissionGranted || action.mode==='receiver'&&!mic?.virtualReady)return null;
    // v1 release enables receiving. Local needs a second localAction after
    // release; Receive can finish with release alone. Never flip Receive first
    // while this device is still the source.
    return action.mode==='source'?mic?.shareAction:mic?.mode==='source'||action.mode==='muted'?mic?.localAction:mic?.receiveAction;
  }
  return null;
}
export function volumeFacts(cards:DeviceCardsView|null,device:string,microphone:boolean) {
  const status=cards?.cards.find(c=>c.device===device)?.status;
  if(microphone){const mic=status?.microphone;return mic?.controlsToken&&mic.forwarding?{token:mic.controlsToken,volume:mic.forwarding.volumePercent,muted:mic.forwarding.muted,exit:false}:null;}
  const exit=cards?.exitVolume;
  if(exit?.receiverDevice===device)return exit.canSet&&exit.versionToken&&exit.volumePercent!==null&&exit.muted!==null
    ?{token:exit.versionToken,volume:exit.volumePercent,muted:exit.muted,exit:true}:null;
  const sp=status?.speaker;
  return sp?.forwardingControlsToken&&sp.forwarding?{token:sp.forwardingControlsToken,volume:sp.forwarding.volumePercent,muted:sp.forwarding.muted,exit:false}:null;
}
/** Missing owner controls can lag a confirmed mode. Never substitute source
 * gain for output volume, or revive a microphone send after leaving Send. */
export function volumeReadiness(cards:DeviceCardsView|null,device:string,action:Extract<ControlAction,{kind:'speakerVolume'|'microphoneVolume'}>):'ready'|'wait'|'drop' {
  const card=cards?.cards.find(c=>c.device===device),s=card?.status;
  if(!card)return 'ready'; // dispatch keeps the existing offline failure
  if(!s)return 'wait';
  if(action.kind==='microphoneVolume') {
    if(!s.microphone)return 'drop';
    if(s.microphone.mode!=='source')return 'drop';
  } else {
    if((s.speaker.icon==='exit')!==action.output)return 'drop';
    if(action.output) {
      const exit=cards?.exitVolume;
      if(exit?.receiverDevice!==device)return 'wait';
      if(exit.condition==='unsupported')return 'drop';
    } else if(cards?.exitVolume?.receiverDevice===device)return 'wait';
  }
  return volumeFacts(cards,device,action.kind==='microphoneVolume')?'ready':'wait';
}
export function controlObservation(cards:DeviceCardsView|null,device:string,action:ControlAction):ControlObservation {
  const card=cards?.cards.find(c=>c.device===device),s=card?.status;
  if(!card)return {token:null,matches:false};
  if(action.kind==='speakerVolume'||action.kind==='microphoneVolume'){
    const facts=volumeFacts(cards,device,action.kind==='microphoneVolume');
    return {volumeReadiness:volumeReadiness(cards,device,action),token:facts?.token??null,matches:!!facts&&(action.kind!=='speakerVolume'||facts.exit===action.output)&&facts.volume===action.volume&&facts.muted===action.muted};
  }
  const command=controlCommand(card,action);
  const token=command?.kind==='invoke'?command.token:null;
  if(action.kind==='speaker')return {token,matches:s?.speaker.icon===action.mode};
  if(action.kind==='microphone'){
    const mic=s?.microphone;
    const mode=!mic?null:mic.mode==='source'?'source':mic.virtualReady&&(mic.mode==='idle'||mic.mode==='receiver')?'receiver':'muted';
    return {token,matches:mode===action.mode,intermediate:mic?.mode==='source'&&action.mode==='muted',microphoneMode:mode??undefined};
  }
  const desired=action.desired;
  switch(action.resource){
    case 'pause':return {token,matches:card.pause?.paused===desired};
    case 'game':return {token,matches:s?.game.preference===desired};
    case 'screen':return {token:desired==='locked'?cards?.revision??null:token,matches:desired==='locked'
      ?s?.cursor.lockedDevice===device||s?.cursor.icon==='locked'
      :!!s&&s.cursor.lockedDevice!==device&&s.cursor.icon!=='locked'&&s.screen.configuredMode===(desired==='shared'?'shared':'local')};
    case 'keyboard':return {token,intermediate:!!s?.keyboard.disabled&&desired!=='disabled'&&s.keyboard.configuredMode!==desired,matches:!!s&&(desired==='disabled'?s.keyboard.disabled===true:!s.keyboard.disabled&&s.keyboard.configuredMode===desired)};
    case 'mouse':case 'clipboard':return {token,matches:s?.[action.resource].icon===desired};
  }
}
